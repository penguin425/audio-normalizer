//! Bounded application of the MPEG-DASH MPD Patch common selector subset.

use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer, XmlVersion};

const PATCH_NAMESPACE: &str = "urn:mpeg:dash:schema:mpd-patch:2020";
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
}

#[derive(Clone, Debug)]
struct XmlElement {
    name: String,
    attributes: Vec<(String, String)>,
    children: Vec<XmlNode>,
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
        attribute: Option<String>,
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
    steps: Vec<ElementStep>,
    attribute: Option<String>,
}

#[derive(Debug)]
struct ElementStep {
    name: String,
    predicates: Vec<Predicate>,
}

#[derive(Debug)]
enum Predicate {
    Position(usize),
    AttributeEquals(String, String),
    TextEquals(String),
    ChildEquals(String, String),
}

pub(crate) fn apply(base_xml: &[u8], patch_xml: &[u8]) -> Result<AppliedPatch, String> {
    let mut base = parse_xml(base_xml, "MPD")?;
    if local_name(base.name.as_str()) != "MPD" {
        return Err("base document root must be MPD".into());
    }
    let patch_root = parse_xml(patch_xml, "MPD Patch")?;
    let patch = parse_patch(&patch_root)?;
    for (index, operation) in patch.operations.iter().enumerate() {
        apply_operation(&mut base, operation)
            .map_err(|error| format!("patch operation {}: {error}", index + 1))?;
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
                stack.push(new_element(&reader, &element)?);
            }
            Ok(Event::Empty(element)) => {
                element_count += 1;
                if element_count > MAX_ELEMENTS {
                    return Err(format!("{label} exceeds the {MAX_ELEMENTS} element limit"));
                }
                attach(
                    XmlNode::Element(new_element(&reader, &element)?),
                    &mut stack,
                    &mut root,
                )?;
            }
            Ok(Event::Text(text)) => {
                if let Some(parent) = stack.last_mut() {
                    let value = text
                        .decode()
                        .map_err(|error| format!("decode {label} text: {error}"))?;
                    parent.children.push(XmlNode::Text(value.into_owned()));
                }
            }
            Ok(Event::CData(text)) => {
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(XmlNode::Text(
                        String::from_utf8_lossy(text.as_ref()).into_owned(),
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
        Some(XmlNode::Element(element)) => Ok(element),
        Some(XmlNode::Text(_)) | None => Err(format!("{label} must have one root element")),
    }
}

fn new_element(reader: &Reader<&[u8]>, start: &BytesStart<'_>) -> Result<XmlElement, String> {
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
    Ok(XmlElement {
        name,
        attributes,
        children: Vec::new(),
    })
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

fn parse_patch(root: &XmlElement) -> Result<Patch, String> {
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
        let selector = Selector::parse(required_attribute(element, "sel")?.as_str())?;
        if element.name.contains(':') {
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
                let attribute = match element.attribute_exact("type") {
                    None => None,
                    Some(value) if value.strip_prefix('@').is_some_and(valid_qname) => {
                        Some(value[1..].to_owned())
                    }
                    Some(value) => {
                        return Err(format!(
                            "unsupported add type {value:?}; only attributes are supported"
                        ))
                    }
                };
                if attribute.is_some() && !matches!(position, AddPosition::Append) {
                    return Err("attribute add must not specify pos".into());
                }
                PatchOperation::Add {
                    selector,
                    position,
                    attribute,
                    content: element.children.clone(),
                }
            }
            "replace" => PatchOperation::Replace {
                selector,
                content: element.children.clone(),
            },
            "remove" => {
                if element.children.iter().any(|node| match node {
                    XmlNode::Element(_) => true,
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
    fn parse(value: &str) -> Result<Self, String> {
        let parts = split_selector(value)?;
        if parts.is_empty() {
            return Err("selector is empty".into());
        }
        let mut steps = Vec::new();
        let mut attribute = None;
        for (index, part) in parts.iter().enumerate() {
            if let Some(name) = part.strip_prefix('@') {
                if index + 1 != parts.len() || !valid_qname(name) {
                    return Err(format!("invalid attribute selector {value:?}"));
                }
                attribute = Some(name.to_owned());
                continue;
            }
            if attribute.is_some() {
                return Err(format!("attribute must terminate selector {value:?}"));
            }
            steps.push(parse_element_step(part)?);
        }
        if steps.is_empty() || local_name(steps[0].name.as_str()) != "MPD" {
            return Err("selector must start at the MPD root".into());
        }
        Ok(Self { steps, attribute })
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

fn parse_element_step(value: &str) -> Result<ElementStep, String> {
    let name_end = value.find('[').unwrap_or(value.len());
    let name = &value[..name_end];
    if name.is_empty() || !valid_qname_or_wildcard(name) {
        return Err(format!("unsupported selector node test {name:?}"));
    }
    let mut predicates = Vec::new();
    let mut rest = &value[name_end..];
    while !rest.is_empty() {
        if !rest.starts_with('[') {
            return Err(format!("malformed selector step {value:?}"));
        }
        let end = find_predicate_end(rest)
            .ok_or_else(|| format!("unclosed predicate in selector step {value:?}"))?;
        predicates.push(parse_predicate(&rest[1..end])?);
        rest = &rest[end + 1..];
    }
    Ok(ElementStep {
        name: name.to_owned(),
        predicates,
    })
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

fn parse_predicate(value: &str) -> Result<Predicate, String> {
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
        if !valid_qname(attribute) {
            return Err(format!("invalid selector attribute {attribute:?}"));
        }
        Ok(Predicate::AttributeEquals(attribute.to_owned(), quoted))
    } else if left == "." {
        Ok(Predicate::TextEquals(quoted))
    } else if valid_qname(left) {
        Ok(Predicate::ChildEquals(left.to_owned(), quoted))
    } else {
        Err(format!("unsupported selector predicate [{value}]"))
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

fn valid_qname_or_wildcard(value: &str) -> bool {
    value == "*" || valid_qname(value)
}

fn valid_qname(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(':')
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

fn apply_operation(root: &mut XmlElement, operation: &PatchOperation) -> Result<(), String> {
    match operation {
        PatchOperation::Add {
            selector,
            position,
            attribute,
            content,
        } => apply_add(root, selector, *position, attribute.as_deref(), content),
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
    attribute: Option<&str>,
    content: &[XmlNode],
) -> Result<(), String> {
    let target = locate(root, selector)?;
    if selector.attribute.is_some() {
        return Err("add selector must locate an element".into());
    }
    if let Some(attribute) = attribute {
        let value = content_text(content)?;
        let element = element_mut_at(root, &target)?;
        if element.attribute_exact(attribute).is_some() {
            return Err(format!("attribute {attribute} already exists"));
        }
        element.attributes.push((attribute.to_owned(), value));
        return Ok(());
    }
    let content = patch_content(content);
    if content.is_empty() {
        return Err("element add must contain at least one node".into());
    }
    match position {
        AddPosition::Append | AddPosition::Prepend => {
            let element = element_mut_at(root, &target)?;
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
    if let Some(attribute) = &selector.attribute {
        let value = content_text(content)?;
        let element = element_mut_at(root, &target)?;
        let (_, existing) = element
            .attributes
            .iter_mut()
            .find(|(name, _)| name == attribute)
            .ok_or_else(|| format!("attribute {attribute} was not found"))?;
        *existing = value;
        return Ok(());
    }
    let content = patch_content(content);
    if target.is_empty() {
        return Err("the MPD root element cannot be replaced".into());
    }
    if content.len() != 1 || !matches!(content[0], XmlNode::Element(_)) {
        return Err("element replace requires exactly one element".into());
    }
    let (parent_path, index) = split_parent(&target)?;
    let parent = element_mut_at(root, parent_path)?;
    parent.children[index] = content.into_iter().next().expect("one replacement node");
    Ok(())
}

fn apply_remove(
    root: &mut XmlElement,
    selector: &Selector,
    whitespace: RemoveWhitespace,
) -> Result<(), String> {
    let target = locate(root, selector)?;
    if let Some(attribute) = &selector.attribute {
        if !matches!(whitespace, RemoveWhitespace::None) {
            return Err("attribute remove must not specify ws".into());
        }
        let element = element_mut_at(root, &target)?;
        let index = element
            .attributes
            .iter()
            .position(|(name, _)| name == attribute)
            .ok_or_else(|| format!("attribute {attribute} was not found"))?;
        element.attributes.remove(index);
        return Ok(());
    }
    let (parent_path, mut index) = split_parent(&target)?;
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

fn locate(root: &XmlElement, selector: &Selector) -> Result<Vec<usize>, String> {
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
                .filter_map(|(index, node)| match node {
                    XmlNode::Element(element) if node_test_matches(element, &step.name) => {
                        let mut path = path.clone();
                        path.push(index);
                        Some(path)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            for predicate in &step.predicates {
                match predicate {
                    Predicate::Position(position) => {
                        candidates = candidates.get(position - 1).cloned().into_iter().collect();
                    }
                    _ => candidates.retain(|path| {
                        element_at(root, path)
                            .is_ok_and(|element| predicate_matches(element, predicate))
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
    if let Some(attribute) = &selector.attribute {
        let element = element_at(root, &target)?;
        let count = element
            .attributes
            .iter()
            .filter(|(name, _)| name == attribute)
            .count();
        if count != 1 {
            return Err(format!(
                "selector must locate exactly one attribute (matched {count})"
            ));
        }
    }
    Ok(target)
}

fn step_matches(element: &XmlElement, step: &ElementStep) -> bool {
    node_test_matches(element, &step.name)
        && step.predicates.iter().all(|predicate| match predicate {
            Predicate::Position(position) => *position == 1,
            _ => predicate_matches(element, predicate),
        })
}

fn node_test_matches(element: &XmlElement, expected: &str) -> bool {
    expected == "*" || qname_matches(element.name.as_str(), expected)
}

fn predicate_matches(element: &XmlElement, predicate: &Predicate) -> bool {
    match predicate {
        Predicate::Position(_) => true,
        Predicate::AttributeEquals(name, expected) => element
            .attributes
            .iter()
            .any(|(actual, value)| actual == name && value == expected),
        Predicate::TextEquals(expected) => element.string_value() == *expected,
        Predicate::ChildEquals(name, expected) => element.children.iter().any(|child| {
            matches!(child, XmlNode::Element(child)
                if node_test_matches(child, name) && child.string_value() == *expected)
        }),
    }
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
            XmlNode::Element(_) => {
                return Err("attribute replacement must contain only text".into())
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
        }
    }
}

fn qname_matches(actual: &str, expected: &str) -> bool {
    local_name(actual) == local_name(expected)
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
    fn rejects_ambiguous_or_unsupported_patch_targets() {
        let ambiguous = br#"<Patch xmlns="urn:mpeg:dash:schema:mpd-patch:2020"
 mpdId="live" originalPublishTime="2026-07-29T00:00:00Z"
 publishTime="2026-07-29T00:00:02Z"><remove sel="/MPD/Period/AdaptationSet/SegmentTemplate/SegmentTimeline/S"/></Patch>"#;
        assert!(apply(BASE.as_bytes(), ambiguous)
            .unwrap_err()
            .contains("exactly one"));

        let unsupported = br#"<Patch xmlns="urn:mpeg:dash:schema:mpd-patch:2020"
 mpdId="live" originalPublishTime="2026-07-29T00:00:00Z"
 publishTime="2026-07-29T00:00:02Z"><remove sel="/MPD/comment()[1]"/></Patch>"#;
        assert!(apply(BASE.as_bytes(), unsupported)
            .unwrap_err()
            .contains("unsupported selector"));
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
}
