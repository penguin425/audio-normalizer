//! Bounded semantic validation for EBU QC Data Model 2026-04 reports.
//!
//! XML Schema validation and data-model semantic validation are distinct. The
//! authoritative XSDs are shipped under `schema/ebu-qc-2026-04`; this module
//! enforces the cross-element rules from the EBU compliance checklist and the
//! optional Scenario 1 guidance that XSD validation alone cannot express.

use crate::ebu_qc_report::{EBU_QC_REPORT_NAMESPACE, EBU_QC_TIMING_NAMESPACE};
use quick_xml::escape::unescape;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::{NsReader, XmlVersion};
use std::collections::{HashMap, HashSet};

/// Commit referenced by the vendored EBU QC 2026-04 schema snapshot.
pub const EBU_QC_2026_04_SOURCE_COMMIT: &str = "c9b04821831a38b91f650449b09a17a8e6092757";
/// SHA-256 of the vendored EBU QC 2026-04 core data-model XSD.
pub const EBU_QC_2026_04_SCHEMA_SHA256: &str =
    "c9e97492d28d8d80d2a26c657f4207ad951fa30ee50badc6cd4377c788f7298b";
/// SHA-256 of the vendored EBU QC 2026-04 timing-extension XSD.
pub const EBU_QC_2026_04_TIMING_SCHEMA_SHA256: &str =
    "53622ffd6dfe578e1f297ec9a1d06cf02ec9692fd950f7c24eb0e0c7171077b2";
/// SHA-256 of the vendored EBU QC Catalogue API v3 XSD.
pub const EBU_QC_2026_04_CATALOGUE_SCHEMA_SHA256: &str =
    "6d83c9f25d28760c5dc06c90c31b8a9fb49b311e49943a5e6ad1ba0b1fa393f6";

/// Maximum accepted EBU QC XML document size.
pub const MAX_EBU_QC_XML_BYTES: usize = 16 * 1024 * 1024;
/// Maximum accepted XML element count.
pub const MAX_EBU_QC_XML_ELEMENTS: usize = 100_000;
/// Maximum accepted XML nesting depth.
pub const MAX_EBU_QC_XML_DEPTH: usize = 64;
/// Maximum total decoded text and attribute bytes.
pub const MAX_EBU_QC_XML_TEXT_BYTES: usize = 8 * 1024 * 1024;
/// Maximum number of Item/ItemResult elements in one report.
pub const MAX_EBU_QC_ITEMS: usize = 10_000;

/// Semantic constraint set applied to an EBU QC report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EbuQcValidationProfile {
    /// General EBU QC Data Model 2026-04 report rules.
    DataModel2026_04,
    /// General rules plus the broadcaster pass/fail Scenario 1 guidance.
    Scenario1,
}

/// Validated high-level facts from an EBU QC report.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EbuQcValidationSummary {
    /// Validated `Report/ReportId` UUID URN.
    pub report_id: String,
    /// Ordered identifiers copied identically by the Report and Profile.
    pub content_identifiers: Vec<String>,
    /// Total matched Item/ItemResult pairs.
    pub item_count: usize,
    /// Pairs interpreted in check mode.
    pub check_item_count: usize,
    /// Pairs interpreted in report mode.
    pub report_item_count: usize,
    /// Validated overall result, absent when the Profile has no checks.
    pub report_check_result: Option<bool>,
    /// Media-playback edit-rate fraction when the timing extension is present.
    pub timing_edit_rate: Option<(u64, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct XmlNode {
    namespace: Option<String>,
    local: String,
    parent: Option<usize>,
    children: Vec<usize>,
    text: String,
}

#[derive(Debug, Clone)]
struct ItemRecord {
    id: String,
    name: Option<String>,
    version: String,
    instance_id: Option<String>,
    used_as: Option<UsedAs>,
    relevance: Option<u8>,
}

#[derive(Debug, Clone)]
struct ResultRecord {
    id: String,
    name: Option<String>,
    version: String,
    instance_id: Option<String>,
    check_result: Option<bool>,
    relevance: Option<u8>,
    execution_status: String,
    node: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsedAs {
    Check,
    Report,
}

#[derive(Debug, Clone, Copy)]
struct ElementSpec {
    name: &'static str,
    min: usize,
    max: usize,
}

const fn required(name: &'static str) -> ElementSpec {
    ElementSpec {
        name,
        min: 1,
        max: 1,
    }
}

const fn optional(name: &'static str) -> ElementSpec {
    ElementSpec {
        name,
        min: 0,
        max: 1,
    }
}

const fn repeated(name: &'static str, min: usize) -> ElementSpec {
    ElementSpec {
        name,
        min,
        max: usize::MAX,
    }
}

/// Parse and semantically validate one bounded EBU QC 2026-04 XML report.
///
/// The caller should additionally validate the original bytes with the
/// vendored `forge-validation.xsd` when formal XML Schema validation is
/// required. This function rejects DTDs, undeclared prefixes, oversized input,
/// illegal report structure, inconsistent identities/results, obsolete
/// `Output/Name=CheckResult`, and invalid timing semantics.
pub fn validate_xml(
    xml: &[u8],
    profile: EbuQcValidationProfile,
) -> Result<EbuQcValidationSummary, String> {
    let nodes = parse_xml(xml)?;
    validate_structure(&nodes)?;
    validate_semantics(&nodes, profile)
}

fn parse_xml(xml: &[u8]) -> Result<Vec<XmlNode>, String> {
    if xml.len() > MAX_EBU_QC_XML_BYTES {
        return Err(format!(
            "EBU QC XML is {} bytes; limit is {MAX_EBU_QC_XML_BYTES}",
            xml.len()
        ));
    }
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().trim_text(false);
    reader.config_mut().check_comments = true;
    let mut nodes = Vec::<XmlNode>::new();
    let mut stack = Vec::<usize>::new();
    let mut roots = 0_usize;
    let mut text_bytes = 0_usize;
    let mut declaration_seen = false;
    let mut content_seen = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                content_seen = true;
                let index = push_node(&reader, &element, stack.last().copied(), &mut nodes)?;
                if stack.is_empty() {
                    roots += 1;
                }
                if stack.len() >= MAX_EBU_QC_XML_DEPTH {
                    return Err(format!(
                        "EBU QC XML nesting depth exceeds {MAX_EBU_QC_XML_DEPTH}"
                    ));
                }
                stack.push(index);
            }
            Ok(Event::Empty(element)) => {
                content_seen = true;
                if stack.len() >= MAX_EBU_QC_XML_DEPTH {
                    return Err(format!(
                        "EBU QC XML nesting depth exceeds {MAX_EBU_QC_XML_DEPTH}"
                    ));
                }
                push_node(&reader, &element, stack.last().copied(), &mut nodes)?;
                if stack.is_empty() {
                    roots += 1;
                }
            }
            Ok(Event::Text(text)) => {
                content_seen = true;
                let decoded = text.xml_content(XmlVersion::Implicit1_0);
                let value =
                    unescape(&decoded).map_err(|error| format!("EBU QC XML entity: {error}"))?;
                add_text(&mut nodes, stack.last().copied(), &value, &mut text_bytes)?;
            }
            Ok(Event::CData(text)) => {
                content_seen = true;
                let value = text.xml_content(XmlVersion::Implicit1_0);
                add_text(&mut nodes, stack.last().copied(), &value, &mut text_bytes)?;
            }
            Ok(Event::GeneralRef(reference)) => {
                content_seen = true;
                let encoded = format!("&{};", reference.xml_content(XmlVersion::Implicit1_0));
                let value =
                    unescape(&encoded).map_err(|error| format!("EBU QC XML entity: {error}"))?;
                add_text(&mut nodes, stack.last().copied(), &value, &mut text_bytes)?;
            }
            Ok(Event::Decl(declaration)) => {
                if declaration_seen || content_seen || !stack.is_empty() {
                    return Err("EBU QC XML declaration must occur once at the start".into());
                }
                let version = declaration
                    .version()
                    .map_err(|error| format!("EBU QC XML declaration: {error}"))?;
                if version.as_ref() != "1.0" {
                    return Err("EBU QC XML declaration must use XML 1.0".into());
                }
                if let Some(encoding) = declaration.encoding() {
                    let encoding =
                        encoding.map_err(|error| format!("EBU QC XML declaration: {error}"))?;
                    if !encoding.as_ref().eq_ignore_ascii_case("UTF-8") {
                        return Err("EBU QC XML declaration encoding must be UTF-8".into());
                    }
                }
                declaration_seen = true;
                content_seen = true;
            }
            Ok(Event::DocType(_)) => return Err("EBU QC XML must not contain a DTD".into()),
            Ok(Event::End(element)) => {
                content_seen = true;
                let index = stack
                    .pop()
                    .ok_or_else(|| "EBU QC XML has an unmatched closing element".to_string())?;
                let (namespace, local) = resolved_element_name(&reader, element.name())?;
                if nodes[index].namespace != namespace || nodes[index].local != local {
                    return Err(format!(
                        "EBU QC XML closing element {local} does not match {}",
                        nodes[index].local
                    ));
                }
            }
            Ok(Event::Comment(comment)) => {
                content_seen = true;
                add_total(&mut text_bytes, comment.as_ref().len())?;
            }
            Ok(Event::PI(instruction)) => {
                content_seen = true;
                add_total(&mut text_bytes, instruction.as_ref().len())?;
            }
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(format!(
                    "EBU QC XML at byte {}: {error}",
                    reader.error_position()
                ))
            }
        }
    }
    if !stack.is_empty() {
        return Err("EBU QC XML ended with unclosed elements".into());
    }
    if roots != 1 {
        return Err(format!(
            "EBU QC XML must contain exactly one root element, found {roots}"
        ));
    }
    Ok(nodes)
}

fn push_node(
    reader: &NsReader<&[u8]>,
    element: &quick_xml::events::BytesStart<'_>,
    parent: Option<usize>,
    nodes: &mut Vec<XmlNode>,
) -> Result<usize, String> {
    if nodes.len() >= MAX_EBU_QC_XML_ELEMENTS {
        return Err(format!(
            "EBU QC XML element count exceeds {MAX_EBU_QC_XML_ELEMENTS}"
        ));
    }
    let (namespace, local) = resolved_element_name(reader, element.name())?;
    let mut attribute_bytes = 0_usize;
    let mut attributes = 0_usize;
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| format!("EBU QC XML attribute: {error}"))?;
        let raw_name = attribute.key.as_ref();
        if raw_name == "xmlns" || raw_name.starts_with("xmlns:") {
            continue;
        }
        attributes += 1;
        if attributes > 64 {
            return Err(format!(
                "EBU QC XML element {local} has more than 64 attributes"
            ));
        }
        let value = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|error| format!("EBU QC XML attribute: {error}"))?;
        attribute_bytes = attribute_bytes
            .checked_add(raw_name.len() + value.len())
            .ok_or_else(|| "EBU QC XML attribute size overflow".to_string())?;
        if attribute_bytes > 64 * 1024 {
            return Err(format!(
                "EBU QC XML element {local} attribute data exceeds 65536 bytes"
            ));
        }
        if namespace.as_deref() == Some(EBU_QC_REPORT_NAMESPACE) {
            return Err(format!(
                "EBU QC core element {local} has an attribute not allowed by the 2026-04 XSD"
            ));
        }
    }
    let index = nodes.len();
    nodes.push(XmlNode {
        namespace,
        local,
        parent,
        children: Vec::new(),
        text: String::new(),
    });
    if let Some(parent) = parent {
        nodes[parent].children.push(index);
    }
    Ok(index)
}

fn resolved_element_name(
    reader: &NsReader<&[u8]>,
    name: quick_xml::name::QName<'_>,
) -> Result<(Option<String>, String), String> {
    let (namespace, local) = reader.resolver().resolve_element(name);
    let namespace = match namespace {
        ResolveResult::Unbound => None,
        ResolveResult::Bound(value) => Some(value.as_ref().to_owned()),
        ResolveResult::Unknown(prefix) => {
            return Err(format!(
                "EBU QC XML uses undeclared namespace prefix {}",
                prefix
            ))
        }
    };
    let local = local.as_ref().to_owned();
    Ok((namespace, local))
}

fn add_text(
    nodes: &mut [XmlNode],
    parent: Option<usize>,
    value: &str,
    total: &mut usize,
) -> Result<(), String> {
    add_total(total, value.len())?;
    if let Some(parent) = parent {
        nodes[parent].text.push_str(value);
    } else if !value.trim().is_empty() {
        return Err("EBU QC XML contains text outside its root element".into());
    }
    Ok(())
}

fn add_total(total: &mut usize, amount: usize) -> Result<(), String> {
    *total = total
        .checked_add(amount)
        .ok_or_else(|| "EBU QC XML text size overflow".to_string())?;
    if *total > MAX_EBU_QC_XML_TEXT_BYTES {
        return Err(format!(
            "EBU QC XML decoded text exceeds {MAX_EBU_QC_XML_TEXT_BYTES} bytes"
        ));
    }
    Ok(())
}

fn validate_structure(nodes: &[XmlNode]) -> Result<(), String> {
    let root = nodes
        .iter()
        .position(|node| node.parent.is_none())
        .ok_or_else(|| "EBU QC XML has no root element".to_string())?;
    require_core(nodes, root, "Report")?;
    validate_sequence(
        nodes,
        root,
        &[
            required("ReportId"),
            optional("ReportCreator"),
            required("ExecutionStatus"),
            optional("ErrorDescription"),
            optional("CheckResult"),
            required("ContentId"),
            required("LastModifiedDateTime"),
            optional("ReportGroupId"),
            optional("OverallAnalysisDuration"),
            optional("ToolInformation"),
            required("Profile"),
            required("ItemResults"),
            optional("ExtensionProperties"),
        ],
    )?;
    validate_uuid(text_child(nodes, root, "ReportId")?, "ReportId")?;
    validate_execution_status(text_child(nodes, root, "ExecutionStatus")?)?;
    if let Some(index) = optional_child(nodes, root, "CheckResult")? {
        parse_bool(text(nodes, index), "Report/CheckResult")?;
    }
    validate_content_id(nodes, child(nodes, root, "ContentId")?)?;
    require_nonempty(
        text_child(nodes, root, "LastModifiedDateTime")?,
        "LastModifiedDateTime",
    )?;
    if let Some(index) = optional_child(nodes, root, "ReportCreator")? {
        validate_operator(nodes, index)?;
    }
    if let Some(index) = optional_child(nodes, root, "ToolInformation")? {
        validate_tool(nodes, index)?;
    }
    validate_profile(nodes, child(nodes, root, "Profile")?)?;
    validate_item_results(nodes, child(nodes, root, "ItemResults")?)?;
    if let Some(index) = optional_child(nodes, root, "ExtensionProperties")? {
        validate_extensions(nodes, index)?;
    }
    Ok(())
}

fn validate_profile(nodes: &[XmlNode], index: usize) -> Result<(), String> {
    validate_sequence(
        nodes,
        index,
        &[
            required("ID"),
            optional("Name"),
            required("ContentId"),
            optional("CheckResultRule"),
            optional("Description"),
            required("Scopes"),
            optional("Requestor"),
            optional("DueDate"),
            optional("RelevanceLevel"),
            required("Items"),
            required("ItemDefinitions"),
            optional("ExtensionProperties"),
        ],
    )?;
    validate_uuid(text_child(nodes, index, "ID")?, "Profile/ID")?;
    validate_content_id(nodes, child(nodes, index, "ContentId")?)?;
    if let Some(rule) = optional_child(nodes, index, "CheckResultRule")? {
        if !matches!(text(nodes, rule), "AND" | "OR" | "MinimumRelevance") {
            return Err(format!(
                "Profile/CheckResultRule has invalid value {:?}",
                text(nodes, rule)
            ));
        }
    }
    validate_scopes(nodes, child(nodes, index, "Scopes")?)?;
    if let Some(requestor) = optional_child(nodes, index, "Requestor")? {
        validate_operator(nodes, requestor)?;
    }
    if let Some(level) = optional_child(nodes, index, "RelevanceLevel")? {
        parse_relevance(text(nodes, level), "Profile/RelevanceLevel")?;
    }
    validate_items(nodes, child(nodes, index, "Items")?)?;
    let definitions = child(nodes, index, "ItemDefinitions")?;
    validate_sequence(nodes, definitions, &[repeated("ItemDefinition", 0)])?;
    if let Some(extension) = optional_child(nodes, index, "ExtensionProperties")? {
        validate_extensions(nodes, extension)?;
    }
    Ok(())
}

fn validate_items(nodes: &[XmlNode], index: usize) -> Result<(), String> {
    validate_sequence(nodes, index, &[repeated("Item", 0)])?;
    if nodes[index].children.len() > MAX_EBU_QC_ITEMS {
        return Err(format!("EBU QC report exceeds {MAX_EBU_QC_ITEMS} Items"));
    }
    for &item in &nodes[index].children {
        validate_item(nodes, item)?;
    }
    Ok(())
}

fn validate_item(nodes: &[XmlNode], index: usize) -> Result<(), String> {
    validate_sequence(
        nodes,
        index,
        &[
            required("EBUQCID"),
            optional("EBUQCName"),
            required("EBUQCVersion"),
            optional("InstanceId"),
            optional("Relevance"),
            optional("UsedAs"),
            optional("Track"),
            required("Scopes"),
            required("Inputs"),
            optional("ExtensionProperties"),
        ],
    )?;
    validate_ebu_id(text_child(nodes, index, "EBUQCID")?)?;
    validate_version(text_child(nodes, index, "EBUQCVersion")?)?;
    if let Some(instance) = optional_child(nodes, index, "InstanceId")? {
        validate_uuid(text(nodes, instance), "Item/InstanceId")?;
    }
    if let Some(relevance) = optional_child(nodes, index, "Relevance")? {
        parse_relevance(text(nodes, relevance), "Item/Relevance")?;
    }
    if let Some(used_as) = optional_child(nodes, index, "UsedAs")? {
        parse_used_as(text(nodes, used_as))?;
    }
    validate_scopes(nodes, child(nodes, index, "Scopes")?)?;
    validate_io_container(nodes, child(nodes, index, "Inputs")?, "Input")?;
    if let Some(extension) = optional_child(nodes, index, "ExtensionProperties")? {
        validate_extensions(nodes, extension)?;
    }
    Ok(())
}

fn validate_item_results(nodes: &[XmlNode], index: usize) -> Result<(), String> {
    validate_sequence(nodes, index, &[repeated("ItemResult", 0)])?;
    if nodes[index].children.len() > MAX_EBU_QC_ITEMS {
        return Err(format!(
            "EBU QC report exceeds {MAX_EBU_QC_ITEMS} ItemResults"
        ));
    }
    for &result in &nodes[index].children {
        validate_item_result(nodes, result)?;
    }
    Ok(())
}

fn validate_item_result(nodes: &[XmlNode], index: usize) -> Result<(), String> {
    validate_sequence(
        nodes,
        index,
        &[
            required("EBUQCID"),
            optional("EBUQCName"),
            required("EBUQCVersion"),
            optional("InstanceId"),
            required("AnalysisMethodUsed"),
            required("ExecutionStatus"),
            optional("ErrorDescription"),
            optional("CheckResult"),
            optional("ResultDescription"),
            optional("ToolInformation"),
            optional("Annotation"),
            optional("Relevance"),
            optional("MaxSeverity"),
            optional("VerificationMedia"),
            optional("DetectionMethod"),
            optional("Revision"),
            optional("CreationDateTime"),
            optional("Operator"),
            required("Outputs"),
            optional("ExtensionProperties"),
        ],
    )?;
    validate_ebu_id(text_child(nodes, index, "EBUQCID")?)?;
    validate_version(text_child(nodes, index, "EBUQCVersion")?)?;
    if let Some(instance) = optional_child(nodes, index, "InstanceId")? {
        validate_uuid(text(nodes, instance), "ItemResult/InstanceId")?;
    }
    if !matches!(
        text_child(nodes, index, "AnalysisMethodUsed")?,
        "readout" | "measurement" | "readout and measurement"
    ) {
        return Err("ItemResult/AnalysisMethodUsed is not an EBU 2026-04 value".into());
    }
    validate_execution_status(text_child(nodes, index, "ExecutionStatus")?)?;
    if let Some(check) = optional_child(nodes, index, "CheckResult")? {
        parse_bool(text(nodes, check), "ItemResult/CheckResult")?;
    }
    if let Some(tool) = optional_child(nodes, index, "ToolInformation")? {
        validate_tool(nodes, tool)?;
    }
    if let Some(annotation) = optional_child(nodes, index, "Annotation")? {
        validate_annotation(nodes, annotation)?;
    }
    if let Some(relevance) = optional_child(nodes, index, "Relevance")? {
        parse_relevance(text(nodes, relevance), "ItemResult/Relevance")?;
    }
    if let Some(max_severity) = optional_child(nodes, index, "MaxSeverity")? {
        normalized_non_negative_integer(text(nodes, max_severity), "ItemResult/MaxSeverity")?;
    }
    if let Some(operator) = optional_child(nodes, index, "Operator")? {
        validate_operator(nodes, operator)?;
    }
    validate_io_container(nodes, child(nodes, index, "Outputs")?, "Output")?;
    if let Some(extension) = optional_child(nodes, index, "ExtensionProperties")? {
        validate_extensions(nodes, extension)?;
    }
    Ok(())
}

fn validate_io_container(
    nodes: &[XmlNode],
    index: usize,
    kind: &'static str,
) -> Result<(), String> {
    validate_sequence(nodes, index, &[repeated(kind, 0)])?;
    for &io in &nodes[index].children {
        validate_io(nodes, io, kind, 1)?;
    }
    Ok(())
}

fn validate_io(
    nodes: &[XmlNode],
    index: usize,
    kind: &'static str,
    depth: usize,
) -> Result<(), String> {
    if depth > 2 {
        return Err(format!(
            "EBU QC {kind} nesting exceeds the two levels allowed by Annex C"
        ));
    }
    let mut specs = vec![
        required("Name"),
        optional("Locator"),
        optional("Track"),
        optional("WrapperLocation"),
        optional("Value"),
    ];
    if kind == "Output" {
        specs.extend([
            optional("Annotation"),
            optional("VerificationMedia"),
            optional("Confidence"),
            optional("Severity"),
            optional("Aliases"),
        ]);
    }
    specs.push(repeated(kind, 0));
    specs.push(optional("ExtensionProperties"));
    validate_sequence(nodes, index, &specs)?;
    let name = text_child(nodes, index, "Name")?;
    validate_ebu_id(name)?;
    if kind == "Output" && name.eq_ignore_ascii_case("CheckResult") {
        return Err(
            "EBU QC 2026-04 prohibits Output/Name=CheckResult; use ItemResult/CheckResult".into(),
        );
    }
    if let Some(locator) = optional_child(nodes, index, "Locator")? {
        validate_locator(nodes, locator)?;
    }
    if let Some(annotation) = optional_child(nodes, index, "Annotation")? {
        validate_annotation(nodes, annotation)?;
    }
    if let Some(confidence) = optional_child(nodes, index, "Confidence")? {
        let value = text(nodes, confidence)
            .parse::<f64>()
            .map_err(|_| "Output/Confidence must be a decimal".to_string())?;
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err("Output/Confidence must be between 0 and 1".into());
        }
    }
    if let Some(severity) = optional_child(nodes, index, "Severity")? {
        normalized_non_negative_integer(text(nodes, severity), "Output/Severity")?;
    }
    for nested in children_named(nodes, index, kind) {
        validate_io(nodes, nested, kind, depth + 1)?;
    }
    if let Some(extension) = optional_child(nodes, index, "ExtensionProperties")? {
        validate_extensions(nodes, extension)?;
    }
    Ok(())
}

fn validate_scopes(nodes: &[XmlNode], index: usize) -> Result<(), String> {
    validate_sequence(nodes, index, &[repeated("Scope", 0)])?;
    for &scope in &nodes[index].children {
        validate_locator(nodes, scope)?;
    }
    Ok(())
}

fn validate_locator(nodes: &[XmlNode], index: usize) -> Result<(), String> {
    validate_sequence(
        nodes,
        index,
        &[
            optional("Start"),
            optional("End"),
            optional("SpatialRegion"),
            optional("ExtensionProperties"),
        ],
    )?;
    if let Some(extension) = optional_child(nodes, index, "ExtensionProperties")? {
        validate_extensions(nodes, extension)?;
    }
    Ok(())
}

fn validate_content_id(nodes: &[XmlNode], index: usize) -> Result<(), String> {
    validate_sequence(nodes, index, &[repeated("ContentIdentifier", 1)])?;
    for &identifier in &nodes[index].children {
        validate_sequence(
            nodes,
            identifier,
            &[
                required("ID"),
                optional("Annotation"),
                optional("ExtensionProperties"),
            ],
        )?;
        require_nonempty(text_child(nodes, identifier, "ID")?, "ContentIdentifier/ID")?;
        if let Some(annotation) = optional_child(nodes, identifier, "Annotation")? {
            validate_annotation(nodes, annotation)?;
        }
        if let Some(extension) = optional_child(nodes, identifier, "ExtensionProperties")? {
            validate_extensions(nodes, extension)?;
        }
    }
    Ok(())
}

fn validate_tool(nodes: &[XmlNode], index: usize) -> Result<(), String> {
    validate_sequence(
        nodes,
        index,
        &[
            required("ToolID"),
            required("ToolName"),
            optional("Vendor"),
            optional("URL"),
            optional("Version"),
            optional("ExtensionProperties"),
        ],
    )?;
    require_nonempty(
        text_child(nodes, index, "ToolID")?,
        "ToolInformation/ToolID",
    )?;
    require_nonempty(
        text_child(nodes, index, "ToolName")?,
        "ToolInformation/ToolName",
    )?;
    if let Some(extension) = optional_child(nodes, index, "ExtensionProperties")? {
        validate_extensions(nodes, extension)?;
    }
    Ok(())
}

fn validate_operator(nodes: &[XmlNode], index: usize) -> Result<(), String> {
    validate_sequence(
        nodes,
        index,
        &[
            required("Name"),
            optional("ID"),
            optional("Role"),
            optional("ExtensionProperties"),
        ],
    )?;
    require_nonempty(text_child(nodes, index, "Name")?, "Operator/Name")?;
    if let Some(extension) = optional_child(nodes, index, "ExtensionProperties")? {
        validate_extensions(nodes, extension)?;
    }
    Ok(())
}

fn validate_annotation(nodes: &[XmlNode], index: usize) -> Result<(), String> {
    validate_sequence(
        nodes,
        index,
        &[
            required("Text"),
            optional("Editor"),
            optional("Timestamp"),
            optional("ExtensionProperties"),
        ],
    )?;
    if let Some(editor) = optional_child(nodes, index, "Editor")? {
        validate_operator(nodes, editor)?;
    }
    if let Some(extension) = optional_child(nodes, index, "ExtensionProperties")? {
        validate_extensions(nodes, extension)?;
    }
    Ok(())
}

fn validate_extensions(nodes: &[XmlNode], index: usize) -> Result<(), String> {
    if !nodes[index].text.trim().is_empty() {
        return Err("ExtensionProperties cannot contain direct text".into());
    }
    for &child in &nodes[index].children {
        if nodes[child].namespace.as_deref() == Some(EBU_QC_REPORT_NAMESPACE)
            || nodes[child].namespace.is_none()
        {
            return Err(format!(
                "ExtensionProperties child {} must use a non-core namespace",
                nodes[child].local
            ));
        }
    }
    Ok(())
}

fn validate_sequence(nodes: &[XmlNode], index: usize, specs: &[ElementSpec]) -> Result<(), String> {
    if !nodes[index].text.trim().is_empty() {
        return Err(format!(
            "EBU QC complex element {} contains direct text",
            nodes[index].local
        ));
    }
    let mut counts = vec![0_usize; specs.len()];
    let mut position = 0_usize;
    for &child in &nodes[index].children {
        if nodes[child].namespace.as_deref() != Some(EBU_QC_REPORT_NAMESPACE) {
            return Err(format!(
                "EBU QC element {}/{} has namespace {:?}, expected {}",
                nodes[index].local,
                nodes[child].local,
                nodes[child].namespace,
                EBU_QC_REPORT_NAMESPACE
            ));
        }
        while position < specs.len() && nodes[child].local != specs[position].name {
            if counts[position] < specs[position].min {
                return Err(format!(
                    "EBU QC element {} is missing required child {}",
                    nodes[index].local, specs[position].name
                ));
            }
            position += 1;
        }
        if position == specs.len() {
            return Err(format!(
                "EBU QC element {} has unexpected or out-of-order child {}",
                nodes[index].local, nodes[child].local
            ));
        }
        counts[position] += 1;
        if counts[position] > specs[position].max {
            return Err(format!(
                "EBU QC element {} repeats singleton child {}",
                nodes[index].local, specs[position].name
            ));
        }
    }
    for (position, spec) in specs.iter().enumerate() {
        if counts[position] < spec.min {
            return Err(format!(
                "EBU QC element {} is missing required child {}",
                nodes[index].local, spec.name
            ));
        }
    }
    Ok(())
}

fn validate_semantics(
    nodes: &[XmlNode],
    profile: EbuQcValidationProfile,
) -> Result<EbuQcValidationSummary, String> {
    let root = nodes
        .iter()
        .position(|node| node.parent.is_none())
        .expect("structure validation found a root");
    let profile_node = child(nodes, root, "Profile")?;
    let report_content = content_identifiers(nodes, child(nodes, root, "ContentId")?)?;
    let profile_content = content_identifiers(nodes, child(nodes, profile_node, "ContentId")?)?;
    if report_content != profile_content {
        return Err("Report/ContentId must be identical to Profile/ContentId".into());
    }

    let items_node = child(nodes, profile_node, "Items")?;
    let results_node = child(nodes, root, "ItemResults")?;
    let items = nodes[items_node]
        .children
        .iter()
        .map(|&index| item_record(nodes, index))
        .collect::<Result<Vec<_>, _>>()?;
    let results = nodes[results_node]
        .children
        .iter()
        .map(|&index| result_record(nodes, index))
        .collect::<Result<Vec<_>, _>>()?;
    if items.len() != results.len() {
        return Err(format!(
            "Profile has {} Items but report has {} ItemResults",
            items.len(),
            results.len()
        ));
    }

    validate_duplicate_item_ids(&items)?;
    let mut result_by_key = HashMap::with_capacity(results.len());
    for result in &results {
        let key = (result.id.clone(), result.instance_id.clone());
        if result_by_key.insert(key, result).is_some() {
            return Err(format!(
                "ItemResults repeats identity {} / {:?}",
                result.id, result.instance_id
            ));
        }
    }

    let mut check_values = Vec::<(bool, Option<u8>)>::new();
    let mut check_count = 0_usize;
    let mut report_count = 0_usize;
    for item in &items {
        let key = (item.id.clone(), item.instance_id.clone());
        let result = result_by_key.remove(&key).ok_or_else(|| {
            format!(
                "Item {} / {:?} has no matching ItemResult",
                item.id, item.instance_id
            )
        })?;
        if item.name != result.name || item.version != result.version {
            return Err(format!(
                "Item {} and ItemResult disagree on name or version",
                item.id
            ));
        }
        match item.used_as {
            Some(UsedAs::Check) => {
                check_count += 1;
                let passed = result.check_result.ok_or_else(|| {
                    format!("check-mode ItemResult {} requires CheckResult", item.id)
                })?;
                check_values.push((passed, item.relevance));
            }
            Some(UsedAs::Report) => {
                report_count += 1;
                if result.check_result.is_some() {
                    return Err(format!(
                        "report-mode ItemResult {} must omit CheckResult",
                        item.id
                    ));
                }
                let outputs = child(nodes, result.node, "Outputs")?;
                if nodes[outputs].children.is_empty() {
                    return Err(format!(
                        "report-mode ItemResult {} requires at least one Output",
                        item.id
                    ));
                }
            }
            None => {
                if let Some(check_result) = result.check_result {
                    check_count += 1;
                    check_values.push((check_result, item.relevance));
                } else {
                    report_count += 1;
                    let outputs = child(nodes, result.node, "Outputs")?;
                    if nodes[outputs].children.is_empty() {
                        return Err(format!(
                            "report-mode ItemResult {} requires at least one Output",
                            item.id
                        ));
                    }
                }
            }
        }
    }
    if !result_by_key.is_empty() {
        return Err("ItemResults contains an entry with no matching Profile Item".into());
    }

    let report_status = text_child(nodes, root, "ExecutionStatus")?;
    let expected_status = if results
        .iter()
        .all(|result| result.execution_status == "complete")
    {
        "complete"
    } else {
        "error"
    };
    if report_status != expected_status {
        return Err(format!(
            "Report/ExecutionStatus must be {expected_status} for its ItemResults"
        ));
    }

    let report_check = optional_child(nodes, root, "CheckResult")?
        .map(|index| parse_bool(text(nodes, index), "Report/CheckResult"))
        .transpose()?;
    let rule =
        optional_child(nodes, profile_node, "CheckResultRule")?.map(|index| text(nodes, index));
    let relevance_level = optional_child(nodes, profile_node, "RelevanceLevel")?
        .map(|index| parse_relevance(text(nodes, index), "Profile/RelevanceLevel"))
        .transpose()?;
    if check_values.is_empty() {
        if report_check.is_some() || rule.is_some() {
            return Err(
                "Report CheckResult and Profile CheckResultRule must be omitted without checks"
                    .into(),
            );
        }
        if relevance_level.is_some()
            || items.iter().any(|item| item.relevance.is_some())
            || results.iter().any(|result| result.relevance.is_some())
        {
            return Err("Relevance is not allowed when no check-mode Items exist".into());
        }
    } else {
        let report_check = report_check
            .ok_or_else(|| "Report/CheckResult is required when checks are present".to_string())?;
        let rule = rule.unwrap_or("AND");
        let expected = match rule {
            "AND" | "OR" => {
                if relevance_level.is_some()
                    || items.iter().any(|item| item.relevance.is_some())
                    || results.iter().any(|result| result.relevance.is_some())
                {
                    return Err(
                        "Relevance and RelevanceLevel require CheckResultRule=MinimumRelevance"
                            .into(),
                    );
                }
                if rule == "AND" {
                    check_values.iter().all(|(passed, _)| *passed)
                } else {
                    check_values.iter().any(|(passed, _)| *passed)
                }
            }
            "MinimumRelevance" => {
                let level = relevance_level.ok_or_else(|| {
                    "MinimumRelevance requires Profile/RelevanceLevel".to_string()
                })?;
                for item in &items {
                    let result = results
                        .iter()
                        .find(|result| {
                            result.id == item.id && result.instance_id == item.instance_id
                        })
                        .expect("one-to-one identities validated");
                    if result.check_result.is_some() {
                        if item.relevance.is_none() || result.relevance != item.relevance {
                            return Err(format!(
                                "MinimumRelevance requires matching Relevance on Item and ItemResult {}",
                                item.id
                            ));
                        }
                    } else if item.relevance.is_some() || result.relevance.is_some() {
                        return Err(format!(
                            "MinimumRelevance prohibits Relevance on report-mode Item {}",
                            item.id
                        ));
                    }
                }
                let selected = check_values
                    .iter()
                    .map(|(passed, relevance)| {
                        relevance
                            .map(|relevance| (passed, relevance))
                            .ok_or_else(|| {
                                "MinimumRelevance requires Relevance on every check Item"
                                    .to_string()
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                selected
                    .iter()
                    .filter(|(_, relevance)| *relevance >= level)
                    .all(|(passed, _)| **passed)
            }
            _ => unreachable!("structure validated rule"),
        };
        if report_check != expected {
            return Err(format!(
                "Report/CheckResult is {report_check}, expected {expected} from {rule}"
            ));
        }
    }

    validate_severity_bounds(nodes, &results)?;
    validate_datetimes(nodes)?;
    let timing_edit_rate = validate_timing(nodes, root, profile)?;
    if profile == EbuQcValidationProfile::Scenario1 {
        validate_scenario1(nodes, root, profile_node, &items, &results, check_count)?;
    }

    Ok(EbuQcValidationSummary {
        report_id: text_child(nodes, root, "ReportId")?.to_owned(),
        content_identifiers: report_content,
        item_count: items.len(),
        check_item_count: check_count,
        report_item_count: report_count,
        report_check_result: report_check,
        timing_edit_rate,
    })
}

fn validate_scenario1(
    nodes: &[XmlNode],
    root: usize,
    profile: usize,
    items: &[ItemRecord],
    results: &[ResultRecord],
    check_count: usize,
) -> Result<(), String> {
    if optional_child(nodes, root, "CheckResult")?.is_none() {
        return Err("Scenario 1 requires Report/CheckResult".into());
    }
    if optional_child(nodes, root, "ToolInformation")?.is_none() {
        return Err("Scenario 1 requires Report/ToolInformation".into());
    }
    if check_count == 0 {
        return Err("Scenario 1 requires at least one check-mode Item".into());
    }
    let definitions = child(nodes, profile, "ItemDefinitions")?;
    if !nodes[definitions].children.is_empty() {
        return Err("Scenario 1 requires Profile/ItemDefinitions to be empty".into());
    }
    let mut versions = HashMap::<&str, &str>::new();
    for item in items {
        if let Some(version) = versions.insert(&item.id, &item.version) {
            if version != item.version {
                return Err(format!(
                    "Scenario 1 repeated EBUQCID {} must use one version",
                    item.id
                ));
            }
        }
        if item.used_as.is_none() {
            return Err(format!("Scenario 1 Item {} requires UsedAs", item.id));
        }
        if item.name.as_deref().is_none_or(str::is_empty) {
            return Err(format!("Scenario 1 Item {} requires EBUQCName", item.id));
        }
    }
    let report_tool_present = optional_child(nodes, root, "ToolInformation")?.is_some();
    for result in results {
        if result.name.as_deref().is_none_or(str::is_empty) {
            return Err(format!(
                "Scenario 1 ItemResult {} requires EBUQCName",
                result.id
            ));
        }
        if optional_child(nodes, result.node, "Revision")?.is_some() {
            return Err(format!(
                "Scenario 1 ItemResult {} must omit Revision",
                result.id
            ));
        }
        if !report_tool_present && optional_child(nodes, result.node, "ToolInformation")?.is_none()
        {
            return Err(format!(
                "Scenario 1 ItemResult {} requires ToolInformation",
                result.id
            ));
        }
    }
    Ok(())
}

fn validate_duplicate_item_ids(items: &[ItemRecord]) -> Result<(), String> {
    let mut by_id = HashMap::<&str, Vec<&ItemRecord>>::new();
    let mut keys = HashSet::new();
    for item in items {
        if !keys.insert((item.id.as_str(), item.instance_id.as_deref())) {
            return Err(format!(
                "Profile/Items repeats identity {} / {:?}",
                item.id, item.instance_id
            ));
        }
        by_id.entry(&item.id).or_default().push(item);
    }
    for (id, duplicates) in by_id {
        if duplicates.len() <= 1 {
            continue;
        }
        if duplicates.iter().any(|item| item.instance_id.is_none()) {
            return Err(format!(
                "repeated EBUQCID {id} requires InstanceId on every Item"
            ));
        }
    }
    Ok(())
}

fn validate_severity_bounds(nodes: &[XmlNode], results: &[ResultRecord]) -> Result<(), String> {
    for result in results {
        let maximum = optional_child(nodes, result.node, "MaxSeverity")?
            .map(|index| {
                normalized_non_negative_integer(text(nodes, index), "ItemResult/MaxSeverity")
            })
            .transpose()?;
        let outputs = child(nodes, result.node, "Outputs")?;
        for &output in &nodes[outputs].children {
            validate_output_severity(nodes, output, maximum, &result.id)?;
        }
    }
    Ok(())
}

fn validate_output_severity(
    nodes: &[XmlNode],
    output: usize,
    maximum: Option<&str>,
    item_id: &str,
) -> Result<(), String> {
    if let Some(severity) = optional_child(nodes, output, "Severity")? {
        let severity = normalized_non_negative_integer(text(nodes, severity), "Output/Severity")?;
        if let Some(maximum) = maximum {
            if decimal_integer_is_greater(severity, maximum) {
                return Err(format!(
                    "Output/Severity {severity} exceeds ItemResult/MaxSeverity {maximum} for {item_id}"
                ));
            }
        }
    }
    for nested in children_named(nodes, output, "Output") {
        validate_output_severity(nodes, nested, maximum, item_id)?;
    }
    Ok(())
}

fn item_record(nodes: &[XmlNode], index: usize) -> Result<ItemRecord, String> {
    Ok(ItemRecord {
        id: text_child(nodes, index, "EBUQCID")?.to_owned(),
        name: optional_child(nodes, index, "EBUQCName")?.map(|child| text(nodes, child).to_owned()),
        version: text_child(nodes, index, "EBUQCVersion")?.to_owned(),
        instance_id: optional_child(nodes, index, "InstanceId")?
            .map(|child| text(nodes, child).to_owned()),
        used_as: optional_child(nodes, index, "UsedAs")?
            .map(|child| parse_used_as(text(nodes, child)))
            .transpose()?,
        relevance: optional_child(nodes, index, "Relevance")?
            .map(|child| parse_relevance(text(nodes, child), "Item/Relevance"))
            .transpose()?,
    })
}

fn result_record(nodes: &[XmlNode], index: usize) -> Result<ResultRecord, String> {
    Ok(ResultRecord {
        id: text_child(nodes, index, "EBUQCID")?.to_owned(),
        name: optional_child(nodes, index, "EBUQCName")?.map(|child| text(nodes, child).to_owned()),
        version: text_child(nodes, index, "EBUQCVersion")?.to_owned(),
        instance_id: optional_child(nodes, index, "InstanceId")?
            .map(|child| text(nodes, child).to_owned()),
        check_result: optional_child(nodes, index, "CheckResult")?
            .map(|child| parse_bool(text(nodes, child), "ItemResult/CheckResult"))
            .transpose()?,
        relevance: optional_child(nodes, index, "Relevance")?
            .map(|child| parse_relevance(text(nodes, child), "ItemResult/Relevance"))
            .transpose()?,
        execution_status: text_child(nodes, index, "ExecutionStatus")?.to_owned(),
        node: index,
    })
}

fn validate_timing(
    nodes: &[XmlNode],
    root: usize,
    profile: EbuQcValidationProfile,
) -> Result<Option<(u64, u64)>, String> {
    let root_extension = optional_child(nodes, root, "ExtensionProperties")?;
    let timing = nodes
        .iter()
        .enumerate()
        .filter_map(|(index, node)| {
            (node.namespace.as_deref() == Some(EBU_QC_TIMING_NAMESPACE)
                && node.local == "TimingExtensionMediaPlaybackEditUnits")
                .then_some(index)
        })
        .collect::<Vec<_>>();
    if timing.len() > 1 {
        return Err("EBU QC report contains more than one Timing Extension".into());
    }
    if let Some(&timing) = timing.first() {
        if nodes[timing].parent != root_extension {
            return Err(
                "Timing Extension must be a direct child of Report/ExtensionProperties".into(),
            );
        }
    }
    if profile == EbuQcValidationProfile::Scenario1 && timing.len() != 1 {
        return Err("Scenario 1 requires exactly one Timing Extension".into());
    }
    let Some(&timing) = timing.first() else {
        return Ok(None);
    };
    if !nodes[timing].text.trim().is_empty() || nodes[timing].children.len() != 1 {
        return Err("Timing Extension must contain exactly one EditRate".into());
    }
    let edit_rate = nodes[timing].children[0];
    if nodes[edit_rate].namespace.as_deref() != Some(EBU_QC_TIMING_NAMESPACE)
        || nodes[edit_rate].local != "EditRate"
        || !nodes[edit_rate].children.is_empty()
    {
        return Err("Timing Extension must contain one timing-namespace EditRate".into());
    }
    let (numerator, denominator) = text(nodes, edit_rate)
        .split_once('/')
        .ok_or_else(|| "Timing EditRate must have numerator/denominator form".to_string())?;
    let numerator = positive_integer(numerator, "Timing EditRate numerator")?;
    let denominator = positive_integer(denominator, "Timing EditRate denominator")?;
    for (index, node) in nodes.iter().enumerate() {
        if node.namespace.as_deref() != Some(EBU_QC_REPORT_NAMESPACE)
            || !matches!(node.local.as_str(), "Locator" | "Scope")
        {
            continue;
        }
        let start = optional_child(nodes, index, "Start")?
            .map(|child| non_negative_integer(text(nodes, child), "Locator/Start"))
            .transpose()?;
        let end = optional_child(nodes, index, "End")?
            .map(|child| non_negative_integer(text(nodes, child), "Locator/End"))
            .transpose()?;
        if let (Some(start), Some(end)) = (start, end) {
            if end < start {
                return Err(format!("Locator End {end} is before Start {start}"));
            }
        }
    }
    Ok(Some((numerator, denominator)))
}

fn validate_datetimes(nodes: &[XmlNode]) -> Result<(), String> {
    for node in nodes {
        if node.namespace.as_deref() == Some(EBU_QC_REPORT_NAMESPACE)
            && matches!(
                node.local.as_str(),
                "LastModifiedDateTime" | "DueDate" | "CreationDateTime" | "Timestamp"
            )
            && !has_timezone(node.text.trim())
        {
            return Err(format!(
                "EBU QC dateTime {} must include a timezone",
                node.local
            ));
        }
    }
    Ok(())
}

fn has_timezone(value: &str) -> bool {
    let value = value.trim();
    if value.ends_with('Z') {
        return value.contains('T');
    }
    if !value.contains('T') || value.len() < 6 {
        return false;
    }
    let suffix = &value[value.len() - 6..];
    matches!(suffix.as_bytes(), [b'+' | b'-', a, b, b':', c, d]
        if a.is_ascii_digit() && b.is_ascii_digit() && c.is_ascii_digit() && d.is_ascii_digit())
}

fn content_identifiers(nodes: &[XmlNode], index: usize) -> Result<Vec<String>, String> {
    nodes[index]
        .children
        .iter()
        .map(|&identifier| Ok(text_child(nodes, identifier, "ID")?.to_owned()))
        .collect()
}

fn require_core(nodes: &[XmlNode], index: usize, local: &str) -> Result<(), String> {
    if nodes[index].namespace.as_deref() != Some(EBU_QC_REPORT_NAMESPACE)
        || nodes[index].local != local
    {
        return Err(format!(
            "EBU QC root must be {{{EBU_QC_REPORT_NAMESPACE}}}{local}"
        ));
    }
    Ok(())
}

fn child(nodes: &[XmlNode], parent: usize, name: &str) -> Result<usize, String> {
    optional_child(nodes, parent, name)?.ok_or_else(|| {
        format!(
            "EBU QC element {} is missing required child {name}",
            nodes[parent].local
        )
    })
}

fn optional_child(nodes: &[XmlNode], parent: usize, name: &str) -> Result<Option<usize>, String> {
    let matches = children_named(nodes, parent, name);
    match matches.as_slice() {
        [] => Ok(None),
        [index] => Ok(Some(*index)),
        _ => Err(format!(
            "EBU QC element {} repeats singleton child {name}",
            nodes[parent].local
        )),
    }
}

fn children_named(nodes: &[XmlNode], parent: usize, name: &str) -> Vec<usize> {
    nodes[parent]
        .children
        .iter()
        .copied()
        .filter(|&index| {
            nodes[index].namespace.as_deref() == Some(EBU_QC_REPORT_NAMESPACE)
                && nodes[index].local == name
        })
        .collect()
}

fn text_child<'a>(nodes: &'a [XmlNode], parent: usize, name: &str) -> Result<&'a str, String> {
    let index = child(nodes, parent, name)?;
    if !nodes[index].children.is_empty() {
        return Err(format!(
            "EBU QC leaf element {name} contains child elements"
        ));
    }
    Ok(text(nodes, index))
}

fn text(nodes: &[XmlNode], index: usize) -> &str {
    nodes[index].text.trim()
}

fn validate_uuid(value: &str, label: &str) -> Result<(), String> {
    let Some(uuid) = value.strip_prefix("urn:uuid:") else {
        return Err(format!("{label} must be a urn:uuid value"));
    };
    let expected = [8_usize, 4, 4, 4, 12];
    let parts = uuid.split('-').collect::<Vec<_>>();
    if parts.len() != expected.len()
        || parts.iter().zip(expected).any(|(part, length)| {
            part.len() != length || !part.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return Err(format!("{label} is not a valid EBU UUID URN"));
    }
    Ok(())
}

fn validate_ebu_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(format!(
            "EBU QC identifier {value:?} has invalid characters"
        ));
    }
    Ok(())
}

fn validate_version(value: &str) -> Result<(), String> {
    let parts = value.split('.').collect::<Vec<_>>();
    if !(2..=3).contains(&parts.len())
        || parts.iter().any(|part| {
            part.is_empty()
                || !part.bytes().all(|byte| byte.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
        })
    {
        return Err(format!(
            "EBUQCVersion {value:?} must be major.minor[.patch]"
        ));
    }
    Ok(())
}

fn validate_execution_status(value: &str) -> Result<(), String> {
    if matches!(value, "complete" | "error") {
        Ok(())
    } else {
        Err(format!("invalid EBU QC ExecutionStatus {value:?}"))
    }
}

fn parse_used_as(value: &str) -> Result<UsedAs, String> {
    match value {
        "check" => Ok(UsedAs::Check),
        "report" => Ok(UsedAs::Report),
        _ => Err(format!("invalid EBU QC UsedAs value {value:?}")),
    }
}

fn parse_bool(value: &str, label: &str) -> Result<bool, String> {
    match value {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(format!("{label} is not an xs:boolean")),
    }
}

fn parse_relevance(value: &str, label: &str) -> Result<u8, String> {
    let value = value
        .parse::<u8>()
        .map_err(|_| format!("{label} must be an integer from 0 to 10"))?;
    if value > 10 {
        return Err(format!("{label} must be an integer from 0 to 10"));
    }
    Ok(value)
}

fn positive_integer(value: &str, label: &str) -> Result<u64, String> {
    let value = non_negative_integer(value, label)?;
    if value == 0 {
        return Err(format!("{label} must be positive"));
    }
    Ok(value)
}

fn non_negative_integer(value: &str, label: &str) -> Result<u64, String> {
    let digits = value.strip_prefix('+').unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("{label} must be a non-negative integer"));
    }
    digits
        .parse::<u64>()
        .map_err(|_| format!("{label} is outside the supported range"))
}

fn normalized_non_negative_integer<'a>(value: &'a str, label: &str) -> Result<&'a str, String> {
    let digits = value.strip_prefix('+').unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("{label} must be a non-negative integer"));
    }
    let normalized = digits.trim_start_matches('0');
    Ok(if normalized.is_empty() {
        "0"
    } else {
        normalized
    })
}

fn decimal_integer_is_greater(value: &str, maximum: &str) -> bool {
    value.len() > maximum.len() || (value.len() == maximum.len() && value > maximum)
}

fn require_nonempty(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty() {
        Err(format!("{label} must not be empty"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Report xmlns="tag:qc.ebu.ch,2026-04">
  <ReportId>urn:uuid:00000000-0000-8000-8000-000000000001</ReportId>
  <ExecutionStatus>complete</ExecutionStatus>
  <CheckResult>true</CheckResult>
  <ContentId><ContentIdentifier><ID>urn:sha256:abc</ID></ContentIdentifier></ContentId>
  <LastModifiedDateTime>2026-09-04T00:00:00Z</LastModifiedDateTime>
  <ToolInformation><ToolID>https://example.test/tool</ToolID><ToolName>Tool</ToolName></ToolInformation>
  <Profile>
    <ID>urn:uuid:00000000-0000-8000-8000-000000000002</ID>
    <ContentId><ContentIdentifier><ID>urn:sha256:abc</ID></ContentIdentifier></ContentId>
    <CheckResultRule>AND</CheckResultRule>
    <Scopes/>
    <Items>
      <Item>
        <EBUQCID>0005B</EBUQCID><EBUQCName>Audio Digital Clipping</EBUQCName><EBUQCVersion>2.0</EBUQCVersion>
        <InstanceId>urn:uuid:00000000-0000-8000-8000-000000000003</InstanceId><UsedAs>check</UsedAs><Scopes/><Inputs/>
      </Item>
      <Item>
        <EBUQCID>0010B</EBUQCID><EBUQCName>Loudness</EBUQCName><EBUQCVersion>2.0</EBUQCVersion>
        <InstanceId>urn:uuid:00000000-0000-8000-8000-000000000004</InstanceId><UsedAs>report</UsedAs><Scopes/><Inputs/>
      </Item>
    </Items>
    <ItemDefinitions/>
  </Profile>
  <ItemResults>
    <ItemResult>
      <EBUQCID>0005B</EBUQCID><EBUQCName>Audio Digital Clipping</EBUQCName><EBUQCVersion>2.0</EBUQCVersion>
      <InstanceId>urn:uuid:00000000-0000-8000-8000-000000000003</InstanceId><AnalysisMethodUsed>measurement</AnalysisMethodUsed>
      <ExecutionStatus>complete</ExecutionStatus><CheckResult>true</CheckResult><DetectionMethod>automatic</DetectionMethod><Outputs/>
    </ItemResult>
    <ItemResult>
      <EBUQCID>0010B</EBUQCID><EBUQCName>Loudness</EBUQCName><EBUQCVersion>2.0</EBUQCVersion>
      <InstanceId>urn:uuid:00000000-0000-8000-8000-000000000004</InstanceId><AnalysisMethodUsed>measurement</AnalysisMethodUsed>
      <ExecutionStatus>complete</ExecutionStatus><DetectionMethod>automatic</DetectionMethod><Outputs><Output><Name>LoudnessTargetLevelIntegrated</Name><Value>-23</Value></Output></Outputs>
    </ItemResult>
  </ItemResults>
  <ExtensionProperties><TimingExtensionMediaPlaybackEditUnits xmlns="tag:qc.ebu.ch,2026-04:extensions:timing"><EditRate>48000/1</EditRate></TimingExtensionMediaPlaybackEditUnits></ExtensionProperties>
</Report>"#;

    #[test]
    fn validates_scenario1_cross_element_rules() {
        let summary = validate_xml(VALID.as_bytes(), EbuQcValidationProfile::Scenario1).unwrap();
        assert_eq!(summary.item_count, 2);
        assert_eq!(summary.check_item_count, 1);
        assert_eq!(summary.report_item_count, 1);
        assert_eq!(summary.report_check_result, Some(true));
        assert_eq!(summary.timing_edit_rate, Some((48_000, 1)));

        let implicit_and = VALID.replace("<CheckResultRule>AND</CheckResultRule>", "");
        assert!(validate_xml(implicit_and.as_bytes(), EbuQcValidationProfile::Scenario1).is_ok());
    }

    #[test]
    fn validates_minimum_relevance_and_inferred_check_mode() {
        let relevant = VALID
            .replace(
                "<CheckResultRule>AND</CheckResultRule>",
                "<CheckResultRule>MinimumRelevance</CheckResultRule>",
            )
            .replace(
                "<Scopes/>\n    <Items>",
                "<Scopes/>\n    <RelevanceLevel>5</RelevanceLevel>\n    <Items>",
            )
            .replace(
                "000000000003</InstanceId><UsedAs>check</UsedAs>",
                "000000000003</InstanceId><Relevance>5</Relevance><UsedAs>check</UsedAs>",
            )
            .replace(
                "<CheckResult>true</CheckResult><DetectionMethod>automatic</DetectionMethod>",
                "<CheckResult>true</CheckResult><Relevance>5</Relevance><DetectionMethod>automatic</DetectionMethod>",
            );
        assert!(validate_xml(relevant.as_bytes(), EbuQcValidationProfile::Scenario1).is_ok());

        let inferred_mismatch = relevant.replace("<UsedAs>check</UsedAs>", "").replace(
            "<CheckResult>true</CheckResult><Relevance>5</Relevance>",
            "<CheckResult>true</CheckResult><Relevance>4</Relevance>",
        );
        assert!(validate_xml(
            inferred_mismatch.as_bytes(),
            EbuQcValidationProfile::DataModel2026_04
        )
        .unwrap_err()
        .contains("matching Relevance"));
    }

    #[test]
    fn applies_repeated_version_rule_only_to_scenario1() {
        let mixed_versions = VALID.replace(
            "<EBUQCID>0010B</EBUQCID><EBUQCName>Loudness</EBUQCName><EBUQCVersion>2.0</EBUQCVersion>",
            "<EBUQCID>0005B</EBUQCID><EBUQCName>Loudness</EBUQCName><EBUQCVersion>2.1</EBUQCVersion>",
        );
        assert!(validate_xml(
            mixed_versions.as_bytes(),
            EbuQcValidationProfile::DataModel2026_04
        )
        .is_ok());
        assert!(
            validate_xml(mixed_versions.as_bytes(), EbuQcValidationProfile::Scenario1)
                .unwrap_err()
                .contains("must use one version")
        );
    }

    #[test]
    fn rejects_obsolete_check_result_output() {
        let invalid = VALID.replace(
            "<Outputs/>",
            "<Outputs><Output><Name>CheckResult</Name><Value>true</Value></Output></Outputs>",
        );
        assert!(
            validate_xml(invalid.as_bytes(), EbuQcValidationProfile::Scenario1)
                .unwrap_err()
                .contains("prohibits Output/Name=CheckResult")
        );
    }

    #[test]
    fn rejects_report_mode_check_result_and_bad_timing() {
        let invalid = VALID.replace(
            "<ExecutionStatus>complete</ExecutionStatus><DetectionMethod>automatic</DetectionMethod><Outputs><Output><Name>LoudnessTargetLevelIntegrated</Name>",
            "<ExecutionStatus>complete</ExecutionStatus><CheckResult>true</CheckResult><DetectionMethod>automatic</DetectionMethod><Outputs><Output><Name>LoudnessTargetLevelIntegrated</Name>",
        );
        assert!(
            validate_xml(invalid.as_bytes(), EbuQcValidationProfile::Scenario1)
                .unwrap_err()
                .contains("report-mode")
        );

        let invalid = VALID.replace("<EditRate>48000/1</EditRate>", "<EditRate>0/1</EditRate>");
        assert!(
            validate_xml(invalid.as_bytes(), EbuQcValidationProfile::Scenario1)
                .unwrap_err()
                .contains("must be positive")
        );

        const TIMING: &str = r#"<ExtensionProperties><TimingExtensionMediaPlaybackEditUnits xmlns="tag:qc.ebu.ch,2026-04:extensions:timing"><EditRate>48000/1</EditRate></TimingExtensionMediaPlaybackEditUnits></ExtensionProperties>"#;
        let misplaced = VALID
            .replace(&format!("  {TIMING}\n</Report>"), "</Report>")
            .replace(
                "<ItemDefinitions/>\n  </Profile>",
                &format!("<ItemDefinitions/>\n    {TIMING}\n  </Profile>"),
            );
        assert!(validate_xml(
            misplaced.as_bytes(),
            EbuQcValidationProfile::DataModel2026_04
        )
        .unwrap_err()
        .contains("direct child of Report/ExtensionProperties"));
    }

    #[test]
    fn rejects_output_severity_above_item_maximum() {
        let invalid = VALID.replace(
            "<ExecutionStatus>complete</ExecutionStatus><DetectionMethod>automatic</DetectionMethod><Outputs><Output><Name>LoudnessTargetLevelIntegrated</Name><Value>-23</Value></Output>",
            "<ExecutionStatus>complete</ExecutionStatus><MaxSeverity>2</MaxSeverity><DetectionMethod>automatic</DetectionMethod><Outputs><Output><Name>LoudnessTargetLevelIntegrated</Name><Value>-23</Value><Severity>3</Severity></Output>",
        );
        assert!(
            validate_xml(invalid.as_bytes(), EbuQcValidationProfile::Scenario1)
                .unwrap_err()
                .contains("exceeds ItemResult/MaxSeverity")
        );
    }

    #[test]
    fn rejects_dtd_wrong_namespace_and_oversize() {
        let dtd = VALID.replacen(
            "<Report",
            "<!DOCTYPE Report [<!ENTITY x SYSTEM \"file:///etc/passwd\">]><Report",
            1,
        );
        assert!(
            validate_xml(dtd.as_bytes(), EbuQcValidationProfile::Scenario1)
                .unwrap_err()
                .contains("DTD")
        );
        let namespace = VALID.replace("tag:qc.ebu.ch,2026-04", "tag:qc.ebu.ch,2026-01");
        assert!(
            validate_xml(namespace.as_bytes(), EbuQcValidationProfile::Scenario1)
                .unwrap_err()
                .contains("root must be")
        );
        let oversized = vec![b' '; MAX_EBU_QC_XML_BYTES + 1];
        assert!(
            validate_xml(&oversized, EbuQcValidationProfile::DataModel2026_04)
                .unwrap_err()
                .contains("limit")
        );
    }

    #[test]
    fn official_transition_sample_is_xsd_but_not_semantically_current() {
        let sample =
            include_bytes!("../schema/ebu-qc-2026-04/qc-reports/qc-report-generic-sample.xml");
        assert!(
            validate_xml(sample, EbuQcValidationProfile::DataModel2026_04)
                .unwrap_err()
                .contains("prohibits Output/Name=CheckResult")
        );
    }
}
