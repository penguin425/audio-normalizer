//! Bounded, namespace-aware application of MPEG-DASH MPD Patch operations.

use quick_xml::escape::unescape;
use quick_xml::events::{BytesEnd, BytesPI, BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer, XmlVersion};

const PATCH_NAMESPACE: &str = "urn:mpeg:dash:schema:mpd-patch:2020";
const MPD_NAMESPACE: &str = "urn:mpeg:dash:schema:mpd:2011";
const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NAMESPACE: &str = "http://www.w3.org/2000/xmlns/";
const MAX_ELEMENTS: usize = 200_000;
const MAX_DEPTH: usize = 64;
const MAX_OPERATIONS: usize = 4_096;

#[derive(Debug)]
pub(crate) struct AppliedPatch {
    pub xml: Vec<u8>,
    pub mpd_id: String,
    pub original_publish_time: String,
    pub publish_time: String,
    pub operation_count: usize,
}

#[derive(Clone, Debug)]
enum XmlNode {
    Element(XmlElement),
    Text(String),
    Comment(String),
    ProcessingInstruction { target: String, content: String },
}

#[derive(Clone, Debug)]
struct XmlElement {
    name: String,
    attributes: Vec<(String, String)>,
    children: Vec<XmlNode>,
    namespaces: Vec<(String, String)>,
}

#[derive(Debug)]
struct Patch {
    mpd_id: String,
    original_publish_time: String,
    publish_time: String,
    operations: Vec<PatchOperation>,
}

#[derive(Debug)]
enum PatchOperation {
    Add {
        selector: Selector,
        position: AddPosition,
        insertion_type: Option<InsertionType>,
        content: Vec<XmlNode>,
    },
    Replace {
        selector: Selector,
        content: Vec<XmlNode>,
    },
    Remove {
        selector: Selector,
        whitespace: RemoveWhitespace,
    },
}

#[derive(Clone, Copy, Debug, Default)]
enum AddPosition {
    #[default]
    Append,
    Prepend,
    Before,
    After,
}

#[derive(Clone, Copy, Debug, Default)]
enum RemoveWhitespace {
    #[default]
    None,
    Before,
    After,
    Both,
}

#[derive(Debug)]
struct Selector {
    steps: Vec<NodeStep>,
    terminal: Option<TerminalSelector>,
}

#[derive(Debug)]
struct NodeStep {
    test: NodeTest,
    predicates: Vec<Predicate>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExpandedName {
    namespace: Option<String>,
    local: String,
}

#[derive(Debug)]
enum NameTest {
    Any,
    Name(ExpandedName),
}

#[derive(Debug)]
enum NodeTest {
    Element(NameTest),
    Text,
    Comment,
    ProcessingInstruction(Option<String>),
}

#[derive(Debug)]
enum TerminalSelector {
    Attribute(ExpandedName),
    Namespace(String),
}

#[derive(Debug)]
enum InsertionType {
    Attribute(ExpandedName, String),
    Namespace(String),
}

#[derive(Debug)]
enum Predicate {
    Position(usize),
    AttributeEquals(ExpandedName, String),
    TextEquals(String),
    ChildEquals(ExpandedName, String),
}

pub(crate) fn apply(base_xml: &[u8], patch_xml: &[u8]) -> Result<AppliedPatch, String> {
    let mut base = parse_xml(base_xml, "MPD")?;
    if local_name(base.name.as_str()) != "MPD" {
        return Err("base document root must be MPD".into());
    }
    let base_namespace = element_namespace(&base)
        .ok_or_else(|| "base MPD root namespace cannot be resolved".to_string())?;
    if base_namespace != MPD_NAMESPACE {
        return Err(format!(
            "base MPD root must use the {MPD_NAMESPACE} namespace"
        ));
    }
    let patch_root = parse_xml(patch_xml, "MPD Patch")?;
    let patch = parse_patch(&patch_root, &base_namespace)?;
    for (index, operation) in patch.operations.iter().enumerate() {
        apply_operation(&mut base, operation)
            .map_err(|error| format!("patch operation {}: {error}", index + 1))?;
        refresh_namespaces(&mut base, &base_namespaces())?;
        normalize_text_nodes(&mut base);
    }
    Ok(AppliedPatch {
        xml: serialize_xml(&base)?,
        mpd_id: patch.mpd_id,
        original_publish_time: patch.original_publish_time,
        publish_time: patch.publish_time,
        operation_count: patch.operations.len(),
    })
}

fn parse_xml(bytes: &[u8], label: &str) -> Result<XmlElement, String> {
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(false);
    let mut stack = Vec::<XmlElement>::new();
    let mut root = None;
    let mut element_count = 0usize;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                element_count += 1;
                if element_count > MAX_ELEMENTS {
                    return Err(format!("{label} exceeds the {MAX_ELEMENTS} element limit"));
                }
                if stack.len() >= MAX_DEPTH {
                    return Err(format!("{label} exceeds the {MAX_DEPTH} level depth limit"));
                }
                let inherited = stack
                    .last()
                    .map_or_else(base_namespaces, |parent| parent.namespaces.clone());
                stack.push(new_element(&reader, &element, &inherited)?);
            }
            Ok(Event::Empty(element)) => {
                element_count += 1;
                if element_count > MAX_ELEMENTS {
                    return Err(format!("{label} exceeds the {MAX_ELEMENTS} element limit"));
                }
                if stack.len() >= MAX_DEPTH {
                    return Err(format!("{label} exceeds the {MAX_DEPTH} level depth limit"));
                }
                let inherited = stack
                    .last()
                    .map_or_else(base_namespaces, |parent| parent.namespaces.clone());
                attach(
                    XmlNode::Element(new_element(&reader, &element, &inherited)?),
                    &mut stack,
                    &mut root,
                )?;
            }
            Ok(Event::Text(text)) => {
                let decoded = text
                    .xml10_content()
                    .map_err(|error| format!("decode {label} text: {error}"))?;
                let value = unescape(&decoded)
                    .map_err(|error| format!("unescape {label} text: {error}"))?;
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(XmlNode::Text(value.into_owned()));
                } else if !value.trim().is_empty() {
                    return Err(format!("{label} contains text outside its root element"));
                }
            }
            Ok(Event::CData(text)) => {
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(XmlNode::Text(
                        String::from_utf8_lossy(text.as_ref()).into_owned(),
                    ));
                } else {
                    return Err(format!("{label} contains CDATA outside its root element"));
                }
            }
            Ok(Event::Comment(comment)) => {
                if let Some(parent) = stack.last_mut() {
                    let value = comment
                        .decode()
                        .map_err(|error| format!("decode {label} comment: {error}"))?;
                    parent.children.push(XmlNode::Comment(value.into_owned()));
                }
            }
            Ok(Event::PI(instruction)) => {
                if let Some(parent) = stack.last_mut() {
                    let target = reader
                        .decoder()
                        .decode(instruction.target())
                        .map_err(|error| format!("decode {label} PI target: {error}"))?
                        .into_owned();
                    let content = reader
                        .decoder()
                        .decode(instruction.content())
                        .map_err(|error| format!("decode {label} PI content: {error}"))?
                        .into_owned();
                    parent
                        .children
                        .push(XmlNode::ProcessingInstruction { target, content });
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                let name = reference
                    .xml10_content()
                    .map_err(|error| format!("decode {label} entity reference: {error}"))?;
                let encoded = format!("&{name};");
                let value = unescape(&encoded)
                    .map_err(|error| format!("resolve {label} entity reference: {error}"))?;
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(XmlNode::Text(value.into_owned()));
                } else {
                    return Err(format!(
                        "{label} contains an entity reference outside its root element"
                    ));
                }
            }
            Ok(Event::End(_)) => {
                let element = stack
                    .pop()
                    .ok_or_else(|| format!("{label} has an unmatched closing element"))?;
                attach(XmlNode::Element(element), &mut stack, &mut root)?;
            }
            Ok(Event::DocType(_)) => return Err(format!("{label} must not contain a DTD")),
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(format!(
                    "{label} XML error at byte {}: {error}",
                    reader.error_position()
                ))
            }
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err(format!("{label} has unclosed elements"));
    }
    match root {
        Some(XmlNode::Element(mut element)) => {
            normalize_text_nodes(&mut element);
            Ok(element)
        }
        Some(XmlNode::Text(_))
        | Some(XmlNode::Comment(_))
        | Some(XmlNode::ProcessingInstruction { .. })
        | None => Err(format!("{label} must have one root element")),
    }
}

fn new_element(
    reader: &Reader<&[u8]>,
    start: &BytesStart<'_>,
    inherited_namespaces: &[(String, String)],
) -> Result<XmlElement, String> {
    let name = String::from_utf8_lossy(start.name().as_ref()).into_owned();
    let mut attributes = Vec::new();
    for attribute in start.attributes().with_checks(false) {
        let attribute = attribute.map_err(|error| format!("read XML attribute: {error}"))?;
        let key = String::from_utf8_lossy(attribute.key.as_ref()).into_owned();
        let value = attribute
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
            .map_err(|error| format!("decode XML attribute {key}: {error}"))?
            .into_owned();
        if attributes.iter().any(|(existing, _)| existing == &key) {
            return Err(format!("duplicate XML attribute {key}"));
        }
        attributes.push((key, value));
    }
    let namespaces = namespaces_for_attributes(inherited_namespaces, &attributes)?;
    validate_element_bindings(&name, &attributes, &namespaces)?;
    Ok(XmlElement {
        name,
        attributes,
        children: Vec::new(),
        namespaces,
    })
}

fn validate_element_bindings(
    name: &str,
    attributes: &[(String, String)],
    namespaces: &[(String, String)],
) -> Result<(), String> {
    if !valid_qname(name) || name.starts_with("xmlns:") {
        return Err(format!("invalid XML element QName {name:?}"));
    }
    if let Some(prefix) = qname_prefix(name) {
        if namespace_uri(namespaces, prefix).is_none() {
            return Err(format!("undeclared XML element prefix {prefix:?}"));
        }
    }
    for (name, _) in attributes {
        if name == "xmlns" || name.starts_with("xmlns:") {
            continue;
        }
        if !valid_qname(name) {
            return Err(format!("invalid XML attribute QName {name:?}"));
        }
        if let Some(prefix) = qname_prefix(name) {
            if namespace_uri(namespaces, prefix).is_none() {
                return Err(format!("undeclared XML attribute prefix {prefix:?}"));
            }
        }
    }
    Ok(())
}

fn base_namespaces() -> Vec<(String, String)> {
    vec![("xml".to_owned(), XML_NAMESPACE.to_owned())]
}

fn namespaces_for_attributes(
    inherited: &[(String, String)],
    attributes: &[(String, String)],
) -> Result<Vec<(String, String)>, String> {
    let mut namespaces = inherited.to_vec();
    for (name, value) in attributes {
        let prefix = if name == "xmlns" {
            Some("")
        } else {
            name.strip_prefix("xmlns:")
        };
        let Some(prefix) = prefix else {
            continue;
        };
        if !prefix.is_empty() {
            validate_namespace_prefix_declaration(prefix, value)?;
        }
        if prefix.is_empty() && value == XML_NAMESPACE {
            return Err("the XML namespace must not be the default namespace".into());
        }
        if value == XMLNS_NAMESPACE {
            return Err("the xmlns namespace URI is reserved".into());
        }
        if let Some((_, existing)) = namespaces
            .iter_mut()
            .find(|(existing, _)| existing == prefix)
        {
            if value.is_empty() {
                namespaces.retain(|(existing, _)| existing != prefix);
            } else {
                *existing = value.clone();
            }
        } else if !value.is_empty() {
            namespaces.push((prefix.to_owned(), value.clone()));
        }
    }
    Ok(namespaces)
}

fn validate_namespace_prefix_declaration(prefix: &str, uri: &str) -> Result<(), String> {
    if !valid_ncname(prefix) || prefix.eq_ignore_ascii_case("xmlns") {
        return Err(format!("invalid namespace declaration prefix {prefix:?}"));
    }
    if prefix.eq_ignore_ascii_case("xml") {
        if uri != XML_NAMESPACE {
            return Err("the xml prefix must use the XML namespace URI".into());
        }
    } else if uri.is_empty() {
        return Err(format!(
            "prefixed namespace declaration {prefix:?} must not be empty"
        ));
    } else if uri == XML_NAMESPACE {
        return Err("only the xml prefix may use the XML namespace URI".into());
    } else if uri == XMLNS_NAMESPACE {
        return Err("the xmlns namespace URI is reserved".into());
    }
    Ok(())
}

fn namespace_uri<'a>(namespaces: &'a [(String, String)], prefix: &str) -> Option<&'a str> {
    namespaces
        .iter()
        .find(|(candidate, _)| candidate == prefix)
        .map(|(_, uri)| uri.as_str())
}

fn refresh_namespaces(
    element: &mut XmlElement,
    inherited: &[(String, String)],
) -> Result<(), String> {
    element.namespaces = namespaces_for_attributes(inherited, &element.attributes)?;
    let namespaces = element.namespaces.clone();
    for child in &mut element.children {
        if let XmlNode::Element(child) = child {
            refresh_namespaces(child, &namespaces)?;
        }
    }
    Ok(())
}

fn element_namespace(element: &XmlElement) -> Option<String> {
    let prefix = element
        .name
        .split_once(':')
        .map_or("", |(prefix, _)| prefix);
    namespace_uri(&element.namespaces, prefix).map(str::to_owned)
}

fn element_expanded_name(element: &XmlElement) -> Option<ExpandedName> {
    Some(ExpandedName {
        namespace: element_namespace(element),
        local: local_name(&element.name).to_owned(),
    })
}

fn attribute_expanded_name(element: &XmlElement, lexical: &str) -> Option<ExpandedName> {
    if lexical == "xmlns" || lexical.starts_with("xmlns:") {
        return None;
    }
    let (prefix, local) = lexical
        .split_once(':')
        .map_or((None, lexical), |(prefix, local)| (Some(prefix), local));
    let namespace = match prefix {
        Some(prefix) => Some(namespace_uri(&element.namespaces, prefix)?.to_owned()),
        None => None,
    };
    Some(ExpandedName {
        namespace,
        local: local.to_owned(),
    })
}

fn attribute_index(element: &XmlElement, expected: &ExpandedName) -> Option<usize> {
    element
        .attributes
        .iter()
        .position(|(name, _)| attribute_expanded_name(element, name).as_ref() == Some(expected))
}

fn lexical_attribute_name(
    element: &mut XmlElement,
    expanded: &ExpandedName,
    requested: &str,
) -> Result<String, String> {
    let Some(uri) = expanded.namespace.as_deref() else {
        if requested.contains(':') {
            return Err("an unqualified attribute must not use a prefix".into());
        }
        return Ok(expanded.local.clone());
    };
    if uri == XML_NAMESPACE {
        return Ok(format!("xml:{}", expanded.local));
    }
    let mut matching = element
        .namespaces
        .iter()
        .filter(|(prefix, candidate)| !prefix.is_empty() && candidate == uri)
        .map(|(prefix, _)| prefix.as_str())
        .collect::<Vec<_>>();
    matching.sort_unstable();
    if let Some(prefix) = matching.first() {
        return Ok(format!("{prefix}:{}", expanded.local));
    }
    let requested_prefix = requested
        .split_once(':')
        .map(|(prefix, _)| prefix)
        .ok_or_else(|| "a qualified attribute requires a prefix".to_string())?;
    let prefix = if namespace_uri(&element.namespaces, requested_prefix).is_none() {
        requested_prefix.to_owned()
    } else {
        (1usize..)
            .map(|index| format!("ns{index}"))
            .find(|candidate| namespace_uri(&element.namespaces, candidate).is_none())
            .expect("an unused generated prefix exists")
    };
    element
        .attributes
        .push((format!("xmlns:{prefix}"), uri.to_owned()));
    Ok(format!("{prefix}:{}", expanded.local))
}

fn attach(
    node: XmlNode,
    stack: &mut [XmlElement],
    root: &mut Option<XmlNode>,
) -> Result<(), String> {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(node);
    } else if root.replace(node).is_some() {
        return Err("XML must have exactly one root element".into());
    }
    Ok(())
}

fn parse_patch(root: &XmlElement, target_default_namespace: &str) -> Result<Patch, String> {
    if local_name(root.name.as_str()) != "Patch" {
        return Err("patch document root must be Patch".into());
    }
    if root.attribute_exact("xmlns") != Some(PATCH_NAMESPACE) {
        return Err(format!(
            "Patch must use the {PATCH_NAMESPACE} default namespace"
        ));
    }
    let mpd_id = required_attribute(root, "mpdId")?;
    let original_publish_time = required_attribute(root, "originalPublishTime")?;
    let publish_time = required_attribute(root, "publishTime")?;
    let mut operations = Vec::new();
    for child in &root.children {
        let XmlNode::Element(element) = child else {
            if matches!(child, XmlNode::Text(text) if text.trim().is_empty()) {
                continue;
            }
            return Err("Patch may only contain patch operation elements".into());
        };
        if operations.len() >= MAX_OPERATIONS {
            return Err(format!(
                "Patch exceeds the {MAX_OPERATIONS} operation limit"
            ));
        }
        let selector = Selector::parse(
            required_attribute(element, "sel")?.as_str(),
            &element.namespaces,
            target_default_namespace,
        )?;
        if element.name.contains(':')
            || element_namespace(element).as_deref() != Some(PATCH_NAMESPACE)
        {
            return Err(format!(
                "unsupported namespace-qualified Patch child {}",
                element.name
            ));
        }
        let operation = match element.name.as_str() {
            "add" => {
                let position = match element.attribute_exact("pos") {
                    None => AddPosition::Append,
                    Some("prepend") => AddPosition::Prepend,
                    Some("before") => AddPosition::Before,
                    Some("after") => AddPosition::After,
                    Some(value) => return Err(format!("invalid add pos {value:?}")),
                };
                let insertion_type = match element.attribute_exact("type") {
                    None => None,
                    Some(value) if value.starts_with('@') => Some(InsertionType::Attribute(
                        resolve_qname(&value[1..], &element.namespaces, None)?,
                        value[1..].to_owned(),
                    )),
                    Some(value) if value.starts_with("namespace::") => {
                        let prefix = &value["namespace::".len()..];
                        validate_namespace_prefix(prefix)?;
                        Some(InsertionType::Namespace(prefix.to_owned()))
                    }
                    Some(value) => {
                        return Err(format!(
                        "unsupported add type {value:?}; expected @attribute or namespace::prefix"
                    ))
                    }
                };
                if insertion_type.is_some() && !matches!(position, AddPosition::Append) {
                    return Err("attribute or namespace add must not specify pos".into());
                }
                PatchOperation::Add {
                    selector,
                    position,
                    insertion_type,
                    content: element.children.clone(),
                }
            }
            "replace" => PatchOperation::Replace {
                selector,
                content: element.children.clone(),
            },
            "remove" => {
                if element.children.iter().any(|node| match node {
                    XmlNode::Element(_)
                    | XmlNode::Comment(_)
                    | XmlNode::ProcessingInstruction { .. } => true,
                    XmlNode::Text(text) => !text.trim().is_empty(),
                }) {
                    return Err("remove operation must be empty".into());
                }
                let whitespace = match element.attribute_exact("ws") {
                    None => RemoveWhitespace::None,
                    Some("before") => RemoveWhitespace::Before,
                    Some("after") => RemoveWhitespace::After,
                    Some("both") => RemoveWhitespace::Both,
                    Some(value) => return Err(format!("invalid remove ws {value:?}")),
                };
                PatchOperation::Remove {
                    selector,
                    whitespace,
                }
            }
            name => return Err(format!("unsupported Patch child {name}")),
        };
        operations.push(operation);
    }
    if operations.is_empty() {
        return Err("Patch must contain at least one operation".into());
    }
    Ok(Patch {
        mpd_id,
        original_publish_time,
        publish_time,
        operations,
    })
}

impl Selector {
    fn parse(
        value: &str,
        namespaces: &[(String, String)],
        target_default_namespace: &str,
    ) -> Result<Self, String> {
        let parts = split_selector(value)?;
        if parts.is_empty() {
            return Err("selector is empty".into());
        }
        let mut steps = Vec::new();
        let mut terminal = None;
        for (index, part) in parts.iter().enumerate() {
            if let Some(name) = part.strip_prefix('@') {
                if index + 1 != parts.len() {
                    return Err(format!("invalid attribute selector {value:?}"));
                }
                terminal = Some(TerminalSelector::Attribute(resolve_qname(
                    name, namespaces, None,
                )?));
                continue;
            }
            if let Some(prefix) = part.strip_prefix("namespace::") {
                if index + 1 != parts.len() {
                    return Err(format!("namespace must terminate selector {value:?}"));
                }
                validate_namespace_prefix(prefix)?;
                terminal = Some(TerminalSelector::Namespace(prefix.to_owned()));
                continue;
            }
            if terminal.is_some() {
                return Err(format!(
                    "attribute or namespace must terminate selector {value:?}"
                ));
            }
            let step = parse_node_step(part, namespaces, target_default_namespace)?;
            if !matches!(step.test, NodeTest::Element(_)) && index + 1 != parts.len() {
                return Err(format!(
                    "non-element node must terminate selector {value:?}"
                ));
            }
            steps.push(step);
        }
        let starts_at_mpd = steps.first().is_some_and(|step| {
            matches!(
                &step.test,
                NodeTest::Element(NameTest::Name(name))
                    if name.local == "MPD"
                        && name.namespace.as_deref() == Some(target_default_namespace)
            )
        });
        if !starts_at_mpd {
            return Err("selector must start at the MPD root".into());
        }
        Ok(Self { steps, terminal })
    }
}

fn split_selector(value: &str) -> Result<Vec<String>, String> {
    let value = value.strip_prefix('/').unwrap_or(value);
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut brackets = 0usize;
    let mut quote = None;
    for (index, character) in value.char_indices() {
        match (quote, character) {
            (Some(active), current) if active == current => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(character),
            (None, '[') => brackets += 1,
            (None, ']') => {
                brackets = brackets
                    .checked_sub(1)
                    .ok_or_else(|| format!("unbalanced selector {value:?}"))?
            }
            (None, '/') if brackets == 0 => {
                if index == start {
                    return Err(format!("empty selector step in {value:?}"));
                }
                parts.push(value[start..index].to_owned());
                start = index + 1;
            }
            _ => {}
        }
    }
    if quote.is_some() || brackets != 0 || start == value.len() {
        return Err(format!("malformed selector {value:?}"));
    }
    parts.push(value[start..].to_owned());
    Ok(parts)
}

fn parse_node_step(
    value: &str,
    namespaces: &[(String, String)],
    target_default_namespace: &str,
) -> Result<NodeStep, String> {
    let name_end = value.find('[').unwrap_or(value.len());
    let name = &value[..name_end];
    let test = parse_node_test(name, namespaces, target_default_namespace)?;
    let mut predicates = Vec::new();
    let mut rest = &value[name_end..];
    while !rest.is_empty() {
        if !rest.starts_with('[') {
            return Err(format!("malformed selector step {value:?}"));
        }
        let end = find_predicate_end(rest)
            .ok_or_else(|| format!("unclosed predicate in selector step {value:?}"))?;
        predicates.push(parse_predicate(
            &rest[1..end],
            namespaces,
            target_default_namespace,
        )?);
        rest = &rest[end + 1..];
    }
    if !matches!(test, NodeTest::Element(_))
        && predicates
            .iter()
            .any(|predicate| !matches!(predicate, Predicate::Position(_)))
    {
        return Err(format!(
            "only positional predicates are supported for node test {name:?}"
        ));
    }
    Ok(NodeStep { test, predicates })
}

fn parse_node_test(
    value: &str,
    namespaces: &[(String, String)],
    target_default_namespace: &str,
) -> Result<NodeTest, String> {
    match value {
        "*" => Ok(NodeTest::Element(NameTest::Any)),
        "text()" => Ok(NodeTest::Text),
        "comment()" => Ok(NodeTest::Comment),
        "processing-instruction()" => Ok(NodeTest::ProcessingInstruction(None)),
        _ if value.starts_with("processing-instruction(") && value.ends_with(')') => {
            let target = parse_quoted(&value["processing-instruction(".len()..value.len() - 1])?;
            if !valid_ncname(&target) || target.eq_ignore_ascii_case("xml") {
                return Err(format!("invalid processing-instruction target {target:?}"));
            }
            Ok(NodeTest::ProcessingInstruction(Some(target)))
        }
        "" => Err("selector node test is empty".into()),
        _ => Ok(NodeTest::Element(NameTest::Name(resolve_qname(
            value,
            namespaces,
            Some(target_default_namespace),
        )?))),
    }
}

fn find_predicate_end(value: &str) -> Option<usize> {
    let mut quote = None;
    for (index, character) in value.char_indices().skip(1) {
        match (quote, character) {
            (Some(active), current) if active == current => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(character),
            (None, ']') => return Some(index),
            _ => {}
        }
    }
    None
}

fn parse_predicate(
    value: &str,
    namespaces: &[(String, String)],
    target_default_namespace: &str,
) -> Result<Predicate, String> {
    if let Ok(position) = value.parse::<usize>() {
        if position == 0 {
            return Err("selector positions are one-based".into());
        }
        return Ok(Predicate::Position(position));
    }
    let (left, right) = value
        .split_once('=')
        .ok_or_else(|| format!("unsupported selector predicate [{value}]"))?;
    let quoted = parse_quoted(right.trim())?;
    let left = left.trim();
    if let Some(attribute) = left.strip_prefix('@') {
        Ok(Predicate::AttributeEquals(
            resolve_qname(attribute, namespaces, None)?,
            quoted,
        ))
    } else if left == "." {
        Ok(Predicate::TextEquals(quoted))
    } else {
        Ok(Predicate::ChildEquals(
            resolve_qname(left, namespaces, Some(target_default_namespace))?,
            quoted,
        ))
    }
}

fn parse_quoted(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    if bytes.len() < 2
        || !matches!(bytes[0], b'\'' | b'"')
        || bytes.last().copied() != Some(bytes[0])
    {
        return Err(format!(
            "selector comparison value must be quoted: {value:?}"
        ));
    }
    Ok(value[1..value.len() - 1].to_owned())
}

fn valid_qname(value: &str) -> bool {
    let mut parts = value.split(':');
    let first = parts.next().unwrap_or_default();
    let second = parts.next();
    parts.next().is_none() && valid_ncname(first) && second.is_none_or(valid_ncname)
}

fn valid_ncname(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .next()
            .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && value.chars().all(|character| {
            character == '_'
                || character == '-'
                || character == '.'
                || character.is_ascii_alphanumeric()
        })
}

fn resolve_qname(
    value: &str,
    namespaces: &[(String, String)],
    unprefixed_namespace: Option<&str>,
) -> Result<ExpandedName, String> {
    if !valid_qname(value) {
        return Err(format!("invalid QName {value:?}"));
    }
    let (prefix, local) = value
        .split_once(':')
        .map_or((None, value), |(prefix, local)| (Some(prefix), local));
    let namespace = match prefix {
        Some(prefix) => Some(
            namespace_uri(namespaces, prefix)
                .ok_or_else(|| format!("undeclared namespace prefix {prefix:?}"))?
                .to_owned(),
        ),
        None => unprefixed_namespace.map(str::to_owned),
    };
    Ok(ExpandedName {
        namespace,
        local: local.to_owned(),
    })
}

fn validate_namespace_prefix(prefix: &str) -> Result<(), String> {
    if !valid_ncname(prefix)
        || prefix.eq_ignore_ascii_case("xml")
        || prefix.eq_ignore_ascii_case("xmlns")
    {
        return Err(format!("invalid namespace prefix {prefix:?}"));
    }
    Ok(())
}

fn apply_operation(root: &mut XmlElement, operation: &PatchOperation) -> Result<(), String> {
    match operation {
        PatchOperation::Add {
            selector,
            position,
            insertion_type,
            content,
        } => apply_add(root, selector, *position, insertion_type.as_ref(), content),
        PatchOperation::Replace { selector, content } => apply_replace(root, selector, content),
        PatchOperation::Remove {
            selector,
            whitespace,
        } => apply_remove(root, selector, *whitespace),
    }
}

fn apply_add(
    root: &mut XmlElement,
    selector: &Selector,
    position: AddPosition,
    insertion_type: Option<&InsertionType>,
    content: &[XmlNode],
) -> Result<(), String> {
    let target = locate(root, selector)?;
    if let Some(insertion_type) = insertion_type {
        let LocatedTarget::Node(path) = target else {
            return Err("typed add selector must locate an element".into());
        };
        let element = element_mut_at(root, &path)?;
        match insertion_type {
            InsertionType::Attribute(name, lexical_name) => {
                let value = content_text(content)?;
                if attribute_index(element, name).is_some() {
                    return Err(format!("attribute {} already exists", name.local));
                }
                let lexical_name = lexical_attribute_name(element, name, lexical_name)?;
                element.attributes.push((lexical_name, value));
            }
            InsertionType::Namespace(prefix) => {
                let value = content_text(content)?;
                if value.is_empty() {
                    return Err("namespace URI must not be empty".into());
                }
                validate_namespace_prefix_declaration(prefix, &value)?;
                let declaration = format!("xmlns:{prefix}");
                if element.attribute_exact(&declaration).is_some() {
                    return Err(format!("namespace prefix {prefix} already exists"));
                }
                element.attributes.push((declaration, value));
            }
        }
        return Ok(());
    }
    let LocatedTarget::Node(target) = target else {
        return Err("untyped add selector must locate a node".into());
    };
    let mut content = patch_content(content);
    if content.is_empty() {
        return Err("node add must contain at least one node".into());
    }
    match position {
        AddPosition::Append | AddPosition::Prepend => {
            let element = element_mut_at(root, &target)?;
            prepare_content_namespaces(&mut content, &element.namespaces)?;
            let index = if matches!(position, AddPosition::Prepend) {
                0
            } else {
                element.children.len()
            };
            element.children.splice(index..index, content);
        }
        AddPosition::Before | AddPosition::After => {
            let (parent_path, index) = split_parent(&target)?;
            let parent = element_mut_at(root, parent_path)?;
            prepare_content_namespaces(&mut content, &parent.namespaces)?;
            let insertion = index + usize::from(matches!(position, AddPosition::After));
            parent.children.splice(insertion..insertion, content);
        }
    }
    Ok(())
}

fn apply_replace(
    root: &mut XmlElement,
    selector: &Selector,
    content: &[XmlNode],
) -> Result<(), String> {
    let target = locate(root, selector)?;
    match target {
        LocatedTarget::Attribute(path, name) => {
            let value = content_text(content)?;
            let element = element_mut_at(root, &path)?;
            let index = attribute_index(element, &name)
                .ok_or_else(|| format!("attribute {} was not found", name.local))?;
            element.attributes[index].1 = value;
            Ok(())
        }
        LocatedTarget::Namespace(path, prefix) => {
            let value = content_text(content)?;
            if value.is_empty() {
                return Err("namespace URI must not be empty".into());
            }
            validate_namespace_prefix_declaration(&prefix, &value)?;
            let element = element_mut_at(root, &path)?;
            let declaration = format!("xmlns:{prefix}");
            let (_, existing) = element
                .attributes
                .iter_mut()
                .find(|(name, _)| name == &declaration)
                .ok_or_else(|| format!("namespace prefix {prefix} was not found"))?;
            *existing = value;
            Ok(())
        }
        LocatedTarget::Node(path) => {
            if path.is_empty() {
                return Err("the MPD root element cannot be replaced".into());
            }
            let existing = node_at(root, &path)?.clone();
            let replacement = replacement_node(&existing, content)?;
            let (parent_path, index) = split_parent(&path)?;
            let parent = element_mut_at(root, parent_path)?;
            if let Some(mut node) = replacement {
                prepare_content_namespaces(std::slice::from_mut(&mut node), &parent.namespaces)?;
                parent.children[index] = node;
            } else {
                parent.children.remove(index);
            }
            Ok(())
        }
    }
}

fn apply_remove(
    root: &mut XmlElement,
    selector: &Selector,
    whitespace: RemoveWhitespace,
) -> Result<(), String> {
    let target = locate(root, selector)?;
    match target {
        LocatedTarget::Attribute(path, name) => {
            if !matches!(whitespace, RemoveWhitespace::None) {
                return Err("attribute remove must not specify ws".into());
            }
            let element = element_mut_at(root, &path)?;
            let index = attribute_index(element, &name)
                .ok_or_else(|| format!("attribute {} was not found", name.local))?;
            element.attributes.remove(index);
            Ok(())
        }
        LocatedTarget::Namespace(path, prefix) => {
            if !matches!(whitespace, RemoveWhitespace::None) {
                return Err("namespace remove must not specify ws".into());
            }
            let element = element_mut_at(root, &path)?;
            let declaration = format!("xmlns:{prefix}");
            let index = element
                .attributes
                .iter()
                .position(|(name, _)| name == &declaration)
                .ok_or_else(|| format!("namespace prefix {prefix} was not found"))?;
            if namespace_binding_is_used(element, &prefix) {
                return Err(format!(
                    "namespace prefix {prefix} is still used by the selected subtree"
                ));
            }
            element.attributes.remove(index);
            Ok(())
        }
        LocatedTarget::Node(path) => {
            let kind = node_kind(node_at(root, &path)?);
            if matches!(kind, NodeKind::Text) && !matches!(whitespace, RemoveWhitespace::None) {
                return Err("text remove must not specify ws".into());
            }
            remove_node_at(root, &path, whitespace)
        }
    }
}

fn remove_node_at(
    root: &mut XmlElement,
    path: &[usize],
    whitespace: RemoveWhitespace,
) -> Result<(), String> {
    let (parent_path, mut index) = split_parent(path)?;
    let parent = element_mut_at(root, parent_path)?;
    if matches!(
        whitespace,
        RemoveWhitespace::Before | RemoveWhitespace::Both
    ) {
        if index == 0 || !is_whitespace_text(&parent.children[index - 1]) {
            return Err("ws=before requires preceding whitespace".into());
        }
        parent.children.remove(index - 1);
        index -= 1;
    }
    parent.children.remove(index);
    if matches!(whitespace, RemoveWhitespace::After | RemoveWhitespace::Both) {
        if index >= parent.children.len() || !is_whitespace_text(&parent.children[index]) {
            return Err("ws=after requires following whitespace".into());
        }
        parent.children.remove(index);
    }
    Ok(())
}

#[derive(Debug)]
enum LocatedTarget {
    Node(Vec<usize>),
    Attribute(Vec<usize>, ExpandedName),
    Namespace(Vec<usize>, String),
}

fn locate(root: &XmlElement, selector: &Selector) -> Result<LocatedTarget, String> {
    let first = &selector.steps[0];
    if !step_matches(root, first) {
        return Err("selector does not match the MPD root".into());
    }
    let mut paths = vec![Vec::new()];
    for step in &selector.steps[1..] {
        let mut next = Vec::new();
        for path in &paths {
            let parent = element_at(root, path)?;
            let mut candidates = parent
                .children
                .iter()
                .enumerate()
                .filter_map(|(index, node)| {
                    if node_test_matches(node, &step.test) {
                        let mut path = path.clone();
                        path.push(index);
                        Some(path)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            for predicate in &step.predicates {
                match predicate {
                    Predicate::Position(position) => {
                        candidates = candidates.get(position - 1).cloned().into_iter().collect();
                    }
                    _ => candidates.retain(|path| {
                        node_at(root, path).is_ok_and(|node| {
                            matches!(node, XmlNode::Element(element)
                                if predicate_matches(element, predicate))
                        })
                    }),
                }
            }
            next.extend(candidates);
        }
        paths = next;
    }
    if paths.len() != 1 {
        return Err(format!(
            "selector must locate exactly one node (matched {})",
            paths.len()
        ));
    }
    let target = paths.pop().expect("one selector match");
    match &selector.terminal {
        None => Ok(LocatedTarget::Node(target)),
        Some(TerminalSelector::Attribute(attribute)) => {
            let element = element_at(root, &target)?;
            let count = usize::from(attribute_index(element, attribute).is_some());
            if count != 1 {
                return Err(format!(
                    "selector must locate exactly one attribute (matched {count})"
                ));
            }
            Ok(LocatedTarget::Attribute(target, attribute.clone()))
        }
        Some(TerminalSelector::Namespace(prefix)) => {
            let element = element_at(root, &target)?;
            let declaration = format!("xmlns:{prefix}");
            let count = element
                .attributes
                .iter()
                .filter(|(name, _)| name == &declaration)
                .count();
            if count != 1 {
                return Err(format!(
                    "selector must locate exactly one locally declared namespace (matched {count})"
                ));
            }
            Ok(LocatedTarget::Namespace(target, prefix.clone()))
        }
    }
}

fn step_matches(element: &XmlElement, step: &NodeStep) -> bool {
    matches!(
        &step.test,
        NodeTest::Element(NameTest::Any) | NodeTest::Element(NameTest::Name(_))
    ) && match &step.test {
        NodeTest::Element(NameTest::Any) => true,
        NodeTest::Element(NameTest::Name(expected)) => {
            element_expanded_name(element).as_ref() == Some(expected)
        }
        _ => false,
    } && step.predicates.iter().all(|predicate| match predicate {
        Predicate::Position(position) => *position == 1,
        _ => predicate_matches(element, predicate),
    })
}

fn node_test_matches(node: &XmlNode, test: &NodeTest) -> bool {
    match (node, test) {
        (XmlNode::Element(_), NodeTest::Element(NameTest::Any)) => true,
        (XmlNode::Element(element), NodeTest::Element(NameTest::Name(expected))) => {
            element_expanded_name(element).as_ref() == Some(expected)
        }
        (XmlNode::Text(_), NodeTest::Text) | (XmlNode::Comment(_), NodeTest::Comment) => true,
        (
            XmlNode::ProcessingInstruction { target, .. },
            NodeTest::ProcessingInstruction(expected),
        ) => expected.as_ref().is_none_or(|expected| expected == target),
        _ => false,
    }
}

fn predicate_matches(element: &XmlElement, predicate: &Predicate) -> bool {
    match predicate {
        Predicate::Position(_) => true,
        Predicate::AttributeEquals(name, expected) => attribute_index(element, name)
            .is_some_and(|index| element.attributes[index].1 == *expected),
        Predicate::TextEquals(expected) => element.string_value() == *expected,
        Predicate::ChildEquals(name, expected) => element.children.iter().any(|child| {
            matches!(child, XmlNode::Element(child)
                if element_expanded_name(child).as_ref() == Some(name)
                    && child.string_value() == *expected)
        }),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NodeKind {
    Element,
    Text,
    Comment,
    ProcessingInstruction,
}

fn node_kind(node: &XmlNode) -> NodeKind {
    match node {
        XmlNode::Element(_) => NodeKind::Element,
        XmlNode::Text(_) => NodeKind::Text,
        XmlNode::Comment(_) => NodeKind::Comment,
        XmlNode::ProcessingInstruction { .. } => NodeKind::ProcessingInstruction,
    }
}

fn replacement_node(existing: &XmlNode, content: &[XmlNode]) -> Result<Option<XmlNode>, String> {
    if matches!(existing, XmlNode::Text(_)) {
        let value = content_text(content)?;
        return Ok((!value.is_empty()).then_some(XmlNode::Text(value)));
    }
    let content = patch_content(content);
    if content.len() != 1 {
        return Err(format!(
            "{} replace requires exactly one node of the same type",
            node_kind_label(node_kind(existing))
        ));
    }
    let replacement = content.into_iter().next().expect("one replacement node");
    if node_kind(&replacement) != node_kind(existing) {
        return Err(format!(
            "{} replace requires a {} node",
            node_kind_label(node_kind(existing)),
            node_kind_label(node_kind(existing))
        ));
    }
    Ok(Some(replacement))
}

fn node_kind_label(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Element => "element",
        NodeKind::Text => "text",
        NodeKind::Comment => "comment",
        NodeKind::ProcessingInstruction => "processing-instruction",
    }
}

fn node_at<'a>(root: &'a XmlElement, path: &[usize]) -> Result<&'a XmlNode, String> {
    let (parent_path, index) = split_parent(path)?;
    element_at(root, parent_path)?
        .children
        .get(index)
        .ok_or_else(|| "selector path no longer identifies a node".into())
}

fn prepare_content_namespaces(
    content: &mut [XmlNode],
    inherited: &[(String, String)],
) -> Result<(), String> {
    for node in content {
        if let XmlNode::Element(element) = node {
            prepare_element_namespaces(element, inherited)?;
        }
    }
    Ok(())
}

fn prepare_element_namespaces(
    element: &mut XmlElement,
    inherited: &[(String, String)],
) -> Result<(), String> {
    let (prefix, _) = element
        .name
        .split_once(':')
        .map_or(("", element.name.as_str()), |(prefix, local)| {
            (prefix, local)
        });
    let mut desired_uri = namespace_uri(&element.namespaces, prefix).map(str::to_owned);
    if prefix.is_empty() && desired_uri.as_deref() == Some(PATCH_NAMESPACE) {
        desired_uri = Some(MPD_NAMESPACE.to_owned());
    }
    let local_declaration = if prefix.is_empty() {
        "xmlns".to_owned()
    } else {
        format!("xmlns:{prefix}")
    };
    if !prefix.is_empty() || desired_uri.is_some() {
        let inherited_uri = namespace_uri(inherited, prefix);
        let locally_declared = element
            .attributes
            .iter()
            .any(|(name, _)| name == &local_declaration);
        if inherited_uri != desired_uri.as_deref() && !locally_declared {
            element
                .attributes
                .push((local_declaration, desired_uri.clone().unwrap_or_default()));
        }
    }
    for (name, _) in element.attributes.clone() {
        let Some((attribute_prefix, _)) = name.split_once(':') else {
            continue;
        };
        if attribute_prefix == "xmlns" || attribute_prefix == "xml" {
            continue;
        }
        let desired = namespace_uri(&element.namespaces, attribute_prefix)
            .ok_or_else(|| format!("unresolved content prefix {attribute_prefix:?}"))?;
        let local_declaration = format!("xmlns:{attribute_prefix}");
        let locally_declared = element
            .attributes
            .iter()
            .any(|(name, _)| name == &local_declaration);
        if namespace_uri(inherited, attribute_prefix) != Some(desired) && !locally_declared {
            element
                .attributes
                .push((local_declaration, desired.to_owned()));
        }
    }
    let own_namespaces = namespaces_for_attributes(inherited, &element.attributes)?;
    validate_element_bindings(&element.name, &element.attributes, &own_namespaces)?;
    element.namespaces = own_namespaces;
    let child_scope = element.namespaces.clone();
    for child in &mut element.children {
        if let XmlNode::Element(child) = child {
            prepare_element_namespaces(child, &child_scope)?;
        }
    }
    Ok(())
}

fn namespace_binding_is_used(element: &XmlElement, prefix: &str) -> bool {
    if qname_prefix(&element.name) == Some(prefix)
        || element
            .attributes
            .iter()
            .any(|(name, _)| !name.starts_with("xmlns") && qname_prefix(name) == Some(prefix))
    {
        return true;
    }
    for child in &element.children {
        let XmlNode::Element(child) = child else {
            continue;
        };
        let local_declaration = child.attribute_exact(&format!("xmlns:{prefix}"));
        if local_declaration.is_some() {
            continue;
        }
        if namespace_binding_is_used(child, prefix) {
            return true;
        }
    }
    false
}

fn qname_prefix(name: &str) -> Option<&str> {
    name.split_once(':').map(|(prefix, _)| prefix)
}

fn normalize_text_nodes(element: &mut XmlElement) {
    for child in &mut element.children {
        if let XmlNode::Element(child) = child {
            normalize_text_nodes(child);
        }
    }
    let mut normalized = Vec::with_capacity(element.children.len());
    for node in element.children.drain(..) {
        match node {
            XmlNode::Text(text) if text.is_empty() => {}
            XmlNode::Text(text) => {
                if let Some(XmlNode::Text(previous)) = normalized.last_mut() {
                    previous.push_str(&text);
                } else {
                    normalized.push(XmlNode::Text(text));
                }
            }
            node => normalized.push(node),
        }
    }
    element.children = normalized;
}

fn element_at<'a>(root: &'a XmlElement, path: &[usize]) -> Result<&'a XmlElement, String> {
    let mut element = root;
    for index in path {
        element = match element.children.get(*index) {
            Some(XmlNode::Element(child)) => child,
            _ => return Err("selector path no longer identifies an element".into()),
        };
    }
    Ok(element)
}

fn element_mut_at<'a>(
    root: &'a mut XmlElement,
    path: &[usize],
) -> Result<&'a mut XmlElement, String> {
    let mut element = root;
    for index in path {
        element = match element.children.get_mut(*index) {
            Some(XmlNode::Element(child)) => child,
            _ => return Err("selector path no longer identifies an element".into()),
        };
    }
    Ok(element)
}

fn split_parent(path: &[usize]) -> Result<(&[usize], usize), String> {
    let (&index, parent) = path
        .split_last()
        .ok_or_else(|| "the MPD root element cannot be removed or given a sibling".to_string())?;
    Ok((parent, index))
}

fn patch_content(content: &[XmlNode]) -> Vec<XmlNode> {
    let first_non_whitespace = content
        .iter()
        .position(|node| !is_whitespace_text(node))
        .unwrap_or(content.len());
    let end = content
        .iter()
        .rposition(|node| !is_whitespace_text(node))
        .map_or(first_non_whitespace, |index| index + 1);
    content[first_non_whitespace..end].to_vec()
}

fn content_text(content: &[XmlNode]) -> Result<String, String> {
    let mut value = String::new();
    for node in content {
        match node {
            XmlNode::Text(text) => value.push_str(text),
            XmlNode::Element(_) | XmlNode::Comment(_) | XmlNode::ProcessingInstruction { .. } => {
                return Err("operation value must contain only text".into())
            }
        }
    }
    Ok(value)
}

fn is_whitespace_text(node: &XmlNode) -> bool {
    matches!(node, XmlNode::Text(text) if text.trim().is_empty())
}

fn required_attribute(element: &XmlElement, name: &str) -> Result<String, String> {
    element
        .attribute_exact(name)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            format!(
                "{} requires a non-empty {name} attribute",
                local_name(element.name.as_str())
            )
        })
}

impl XmlElement {
    fn attribute_exact(&self, expected: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(name, _)| name == expected)
            .map(|(_, value)| value.as_str())
    }

    fn string_value(&self) -> String {
        let mut value = String::new();
        append_string_value(self, &mut value);
        value
    }
}

fn append_string_value(element: &XmlElement, output: &mut String) {
    for child in &element.children {
        match child {
            XmlNode::Element(child) => append_string_value(child, output),
            XmlNode::Text(text) => output.push_str(text),
            XmlNode::Comment(_) | XmlNode::ProcessingInstruction { .. } => {}
        }
    }
}

fn local_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

fn serialize_xml(root: &XmlElement) -> Result<Vec<u8>, String> {
    let mut writer = Writer::new(Vec::new());
    write_element(&mut writer, root)?;
    Ok(writer.into_inner())
}

fn write_element(writer: &mut Writer<Vec<u8>>, element: &XmlElement) -> Result<(), String> {
    let mut start = BytesStart::new(element.name.as_str());
    for (name, value) in &element.attributes {
        start.push_attribute((name.as_str(), value.as_str()));
    }
    writer
        .write_event(Event::Start(start))
        .map_err(|error| format!("serialize patched MPD: {error}"))?;
    for child in &element.children {
        match child {
            XmlNode::Element(child) => write_element(writer, child)?,
            XmlNode::Text(text) => writer
                .write_event(Event::Text(BytesText::new(text)))
                .map_err(|error| format!("serialize patched MPD text: {error}"))?,
            XmlNode::Comment(comment) => writer
                .write_event(Event::Comment(BytesText::from_escaped(comment)))
                .map_err(|error| format!("serialize patched MPD comment: {error}"))?,
            XmlNode::ProcessingInstruction { target, content } => writer
                .write_event(Event::PI(BytesPI::new(format!("{target}{content}"))))
                .map_err(|error| {
                    format!("serialize patched MPD processing instruction: {error}")
                })?,
        }
    }
    writer
        .write_event(Event::End(BytesEnd::new(element.name.as_str())))
        .map_err(|error| format!("serialize patched MPD: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" id="live"
 publishTime="2026-07-29T00:00:00Z"><PatchLocation ttl="60">old</PatchLocation>
 <Period id="p0"><AdaptationSet id="a"><SegmentTemplate><SegmentTimeline>
 <S t="0" d="10"/><S t="10" d="10"/></SegmentTimeline></SegmentTemplate>
 </AdaptationSet></Period></MPD>"#;

    #[test]
    fn applies_common_dash_patch_operations_sequentially() {
        let patch = br#"<Patch xmlns="urn:mpeg:dash:schema:mpd-patch:2020"
 mpdId="live" originalPublishTime="2026-07-29T00:00:00Z"
 publishTime="2026-07-29T00:00:02Z">
 <replace sel="/MPD/@publishTime">2026-07-29T00:00:02Z</replace>
 <replace sel="/MPD/PatchLocation[1]"><PatchLocation ttl="60">new</PatchLocation></replace>
 <remove sel="/MPD/Period[@id='p0']/AdaptationSet[@id='a']/SegmentTemplate/SegmentTimeline/S[1]"/>
 <add sel="/MPD/Period[@id='p0']/AdaptationSet[@id='a']/SegmentTemplate/SegmentTimeline" pos="prepend"><S t="10" d="10"/></add>
 <add sel="/MPD/Period[@id='p0']/AdaptationSet[@id='a']/SegmentTemplate/SegmentTimeline"><S t="20" d="10"/></add>
 </Patch>"#;
        let applied = apply(BASE.as_bytes(), patch).unwrap();
        let text = String::from_utf8(applied.xml).unwrap();
        assert_eq!(applied.operation_count, 5);
        assert!(text.contains("publishTime=\"2026-07-29T00:00:02Z\""));
        assert!(text.contains(">new</PatchLocation>"));
        assert!(text.contains("<S t=\"20\" d=\"10\">"));
    }

    #[test]
    fn rejects_ambiguous_or_invalid_patch_targets() {
        let ambiguous = br#"<Patch xmlns="urn:mpeg:dash:schema:mpd-patch:2020"
 mpdId="live" originalPublishTime="2026-07-29T00:00:00Z"
 publishTime="2026-07-29T00:00:02Z"><remove sel="/MPD/Period/AdaptationSet/SegmentTemplate/SegmentTimeline/S"/></Patch>"#;
        assert!(apply(BASE.as_bytes(), ambiguous)
            .unwrap_err()
            .contains("exactly one"));

        let invalid = br#"<Patch xmlns="urn:mpeg:dash:schema:mpd-patch:2020"
 mpdId="live" originalPublishTime="2026-07-29T00:00:00Z"
 publishTime="2026-07-29T00:00:02Z"><remove sel="/MPD/id('missing')"/></Patch>"#;
        assert!(apply(BASE.as_bytes(), invalid)
            .unwrap_err()
            .contains("invalid QName"));
    }

    #[test]
    fn applies_attribute_operations_and_value_predicates() {
        let base = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" id="live"
 publishTime="2026-07-29T00:00:00Z"><Period id="p0"><Label>main</Label></Period></MPD>"#;
        let patch = br#"<Patch xmlns="urn:mpeg:dash:schema:mpd-patch:2020"
 mpdId="live" originalPublishTime="2026-07-29T00:00:00Z"
 publishTime="2026-07-29T00:00:02Z">
 <add sel="/MPD/Period[Label='main']" type="@duration">PT1S</add>
 <replace sel="/MPD/Period[@duration='PT1S']/@duration">PT2S</replace>
 <remove sel="/MPD/Period[@duration='PT2S']/@duration"/>
 </Patch>"#;
        let applied = apply(base, patch).unwrap();
        let text = String::from_utf8(applied.xml).unwrap();
        assert!(!text.contains("duration="));
    }

    #[test]
    fn patches_comment_processing_instruction_and_text_nodes() {
        let base = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" id="live"
 publishTime="2026-07-29T00:00:00Z"><!--old & value--><?audit old?><Label>old</Label></MPD>"#;
        let patch = br#"<Patch xmlns="urn:mpeg:dash:schema:mpd-patch:2020"
 mpdId="live" originalPublishTime="2026-07-29T00:00:00Z"
 publishTime="2026-07-29T00:00:02Z">
 <replace sel="/MPD/comment()[1]"><!--new & value--></replace>
 <replace sel="/MPD/processing-instruction('audit')"><?audit new?></replace>
 <replace sel="/MPD/Label/text()[1]">fresh &amp; clear</replace>
 <add sel="/MPD/Label" pos="after"><!--after--><?check temporary?></add>
 <remove sel="/MPD/processing-instruction('check')"/>
 </Patch>"#;
        let applied = apply(base, patch).unwrap();
        let text = String::from_utf8(applied.xml).unwrap();
        assert!(text.contains("<!--new & value-->"));
        assert!(text.contains("<?audit new?>"));
        assert!(
            text.contains("<Label>fresh &amp; clear</Label><!--after-->"),
            "{text}"
        );
        assert!(!text.contains("&amp;amp;"));
        assert!(!text.contains("temporary"));
    }

    #[test]
    fn resolves_selector_qnames_by_namespace_uri_and_patches_namespace_nodes() {
        let base = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
 xmlns:base="urn:example:metadata" id="live"
 publishTime="2026-07-29T00:00:00Z"><base:Meta base:flag="yes"/></MPD>"#;
        let patch = br#"<Patch xmlns="urn:mpeg:dash:schema:mpd-patch:2020"
 xmlns:m="urn:mpeg:dash:schema:mpd:2011" xmlns:p="urn:example:metadata"
 mpdId="live" originalPublishTime="2026-07-29T00:00:00Z"
 publishTime="2026-07-29T00:00:02Z">
 <add sel="/m:MPD/p:Meta[@p:flag='yes']" type="@p:mode">one</add>
 <replace sel="/MPD/p:Meta/@p:mode">two</replace>
 <remove sel="/MPD/p:Meta/@p:mode"/>
 <add sel="/MPD/p:Meta" type="namespace::temporary">urn:temporary:one</add>
 <replace sel="/MPD/p:Meta/namespace::temporary">urn:temporary:two</replace>
 <remove sel="/MPD/p:Meta/namespace::temporary"/>
 <add sel="/MPD"><p:Added p:value="kept"/></add>
 </Patch>"#;
        let applied = apply(base, patch).unwrap();
        let text = String::from_utf8(applied.xml).unwrap();
        assert!(text.contains("<base:Meta base:flag=\"yes\">"));
        assert!(!text.contains("mode="));
        assert!(!text.contains("xmlns:temporary"));
        assert!(text.contains("<p:Added p:value=\"kept\" xmlns:p=\"urn:example:metadata\">"));
    }

    #[test]
    fn rejects_undeclared_selector_prefix_and_used_namespace_removal() {
        let undeclared = br#"<Patch xmlns="urn:mpeg:dash:schema:mpd-patch:2020"
 mpdId="live" originalPublishTime="2026-07-29T00:00:00Z"
 publishTime="2026-07-29T00:00:02Z"><remove sel="/MPD/x:Meta"/></Patch>"#;
        assert!(apply(BASE.as_bytes(), undeclared)
            .unwrap_err()
            .contains("undeclared namespace prefix"));

        let base = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"
 xmlns:p="urn:example:metadata" id="live"
 publishTime="2026-07-29T00:00:00Z"><p:Meta/></MPD>"#;
        let used = br#"<Patch xmlns="urn:mpeg:dash:schema:mpd-patch:2020"
 mpdId="live" originalPublishTime="2026-07-29T00:00:00Z"
 publishTime="2026-07-29T00:00:02Z"><remove sel="/MPD/namespace::p"/></Patch>"#;
        assert!(apply(base, used).unwrap_err().contains("still used"));
    }

    #[test]
    fn inserts_dash_content_into_a_prefixed_mpd_document() {
        let base = br#"<d:MPD xmlns:d="urn:mpeg:dash:schema:mpd:2011" id="live"
 publishTime="2026-07-29T00:00:00Z"></d:MPD>"#;
        let patch = br#"<Patch xmlns="urn:mpeg:dash:schema:mpd-patch:2020"
 mpdId="live" originalPublishTime="2026-07-29T00:00:00Z"
 publishTime="2026-07-29T00:00:02Z">
 <add sel="/MPD"><Period id="p0"/></add>
 </Patch>"#;
        let applied = apply(base, patch).unwrap();
        let text = String::from_utf8(applied.xml).unwrap();
        assert!(text.contains("<Period id=\"p0\" xmlns=\"urn:mpeg:dash:schema:mpd:2011\">"));
    }
}
