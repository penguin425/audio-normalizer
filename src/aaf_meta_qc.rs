//! Bounded validation of the self-describing AAF MetaDictionary.
//!
//! AAF extension properties are assigned local property identifiers and are
//! only meaningful when their ClassDefinition, PropertyDefinition, and
//! TypeDefinition graphs are interpreted together.  This module validates
//! that graph and checks extension values without loading essence or resolving
//! external locators.

use crate::aaf_object_qc::{StoredObject, StoredProperty};
use crate::container_qc::{check, AuditCheck};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const MAX_FINDINGS: usize = 100;
const MAX_CLASS_DEFINITIONS: usize = 4_096;
const MAX_TYPE_DEFINITIONS: usize = 8_192;
const MAX_PROPERTY_DEFINITIONS: usize = 32_768;
const MAX_MODEL_DEPTH: usize = 64;
const MAX_FIXED_ARRAY_ELEMENTS: u32 = 1_048_576;

const ROOT_CLASS: &str = "b3b398a5-1c90-11d4-8053-080036210804";
const CLASS_DEFINITION: &str = "0d010101-0201-0000-060e-2b3402060101";
const PROPERTY_DEFINITION: &str = "0d010101-0202-0000-060e-2b3402060101";
const TYPE_INTEGER: &str = "0d010101-0204-0000-060e-2b3402060101";
const TYPE_STRONG_REF: &str = "0d010101-0205-0000-060e-2b3402060101";
const TYPE_WEAK_REF: &str = "0d010101-0206-0000-060e-2b3402060101";
const TYPE_ENUM: &str = "0d010101-0207-0000-060e-2b3402060101";
const TYPE_FIXED_ARRAY: &str = "0d010101-0208-0000-060e-2b3402060101";
const TYPE_VAR_ARRAY: &str = "0d010101-0209-0000-060e-2b3402060101";
const TYPE_SET: &str = "0d010101-020a-0000-060e-2b3402060101";
const TYPE_STRING: &str = "0d010101-020b-0000-060e-2b3402060101";
const TYPE_STREAM: &str = "0d010101-020c-0000-060e-2b3402060101";
const TYPE_RECORD: &str = "0d010101-020d-0000-060e-2b3402060101";
const TYPE_RENAME: &str = "0d010101-020e-0000-060e-2b3402060101";
const TYPE_EXT_ENUM: &str = "0d010101-0220-0000-060e-2b3402060101";
const TYPE_INDIRECT: &str = "0d010101-0221-0000-060e-2b3402060101";
const TYPE_OPAQUE: &str = "0d010101-0222-0000-060e-2b3402060101";
const TYPE_CHARACTER: &str = "0d010101-0223-0000-060e-2b3402060101";
const META_DICTIONARY: &str = "0d010101-0225-0000-060e-2b3402060101";
const TYPE_GENERIC_CHARACTER: &str = "0e040101-0000-0000-060e-2b3402060101";

const SF_WEAK_REF: u16 = 0x02;
const SF_WEAK_VECTOR: u16 = 0x12;
const SF_WEAK_SET: u16 = 0x1a;
const SF_STRONG_REF: u16 = 0x22;
const SF_STRONG_VECTOR: u16 = 0x32;
const SF_STRONG_SET: u16 = 0x3a;
const SF_DATA_STREAM: u16 = 0x42;
const SF_DATA: u16 = 0x82;

#[derive(Debug)]
pub(crate) struct MetaAudit {
    pub(crate) checks: Vec<AuditCheck>,
    pub(crate) properties: Value,
}

#[derive(Default)]
struct Findings {
    values: Vec<String>,
    total: usize,
}

impl Findings {
    fn push(&mut self, value: impl Into<String>) {
        self.total += 1;
        if self.values.len() < MAX_FINDINGS {
            self.values.push(value.into());
        }
    }

    fn observed(&self) -> Option<Value> {
        (self.total != 0).then(|| {
            json!({
                "total": self.total,
                "reported": self.values,
            })
        })
    }
}

#[derive(Clone, Debug)]
struct PropertyDef {
    path: PathBuf,
    id: String,
    name: String,
    pid: u16,
    type_id: String,
    optional: bool,
    unique: bool,
}

#[derive(Clone, Debug)]
struct ClassDef {
    path: PathBuf,
    id: String,
    name: String,
    parent: Option<String>,
    concrete: bool,
    properties: Vec<PropertyDef>,
}

#[derive(Clone, Debug)]
struct TypeDef {
    path: PathBuf,
    id: String,
    name: String,
    kind: TypeKind,
}

#[derive(Clone, Debug)]
enum TypeKind {
    Integer {
        size: usize,
        signed: bool,
    },
    StrongRef {
        class_id: String,
    },
    WeakRef {
        class_id: String,
        target_set: Vec<String>,
    },
    Enumeration {
        element_type: String,
        values: HashSet<i64>,
    },
    FixedArray {
        element_type: String,
        count: u32,
    },
    VariableArray {
        element_type: String,
    },
    Set {
        element_type: String,
    },
    String {
        element_type: String,
    },
    Stream,
    Record {
        member_types: Vec<String>,
    },
    Rename {
        renamed_type: String,
    },
    ExtendibleEnumeration {
        values: HashSet<Vec<u8>>,
    },
    Indirect,
    Opaque,
    Character {
        size: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Storage {
    Data(Option<usize>),
    Stream,
    StrongRef,
    WeakRef,
    StrongVector,
    WeakVector,
    StrongSet,
    WeakSet,
}

#[derive(Default)]
struct Model {
    classes: HashMap<String, ClassDef>,
    types: HashMap<String, TypeDef>,
    property_ids: HashSet<String>,
    property_count: usize,
    extension_property_count: usize,
    max_class_depth: usize,
    max_type_depth: usize,
}

pub(crate) fn audit(
    objects: &[StoredObject],
    streams: &HashMap<PathBuf, Vec<u8>>,
    stream_paths: &HashSet<PathBuf>,
    references: &HashMap<(PathBuf, u16), Vec<PathBuf>>,
) -> MetaAudit {
    let by_path: HashMap<&Path, &StoredObject> = objects
        .iter()
        .map(|object| (object.path.as_path(), object))
        .collect();
    let mut definitions = Findings::default();
    let mut values = Findings::default();
    let mut model = build_model(objects, streams, references, &by_path, &mut definitions);
    validate_model(&mut model, &mut definitions);
    let interpreted = validate_extension_values(
        objects,
        stream_paths,
        references,
        &by_path,
        &model,
        &mut values,
    );

    let mut categories = Map::new();
    for typedef in model.types.values() {
        let name = type_category(&typedef.kind);
        let count = categories
            .get(name)
            .and_then(Value::as_u64)
            .unwrap_or_default();
        categories.insert(name.to_owned(), Value::from(count + 1));
    }

    MetaAudit {
        checks: vec![
            check(
                "FORGE-AAF-METADICTIONARY-DEFINITIONS",
                definitions.total == 0,
                if definitions.total == 0 {
                    "AAF ClassDefinition, PropertyDefinition, and TypeDefinition graphs are bounded and coherent"
                } else {
                    "one or more AAF MetaDictionary definitions are malformed or unresolved"
                },
                definitions.observed(),
            ),
            check(
                "FORGE-AAF-EXTENSION-PROPERTY-TYPES",
                values.total == 0,
                if values.total == 0 {
                    "extension properties conform to their dynamically declared AAF types"
                } else {
                    "one or more extension properties do not conform to their dynamically declared AAF types"
                },
                values.observed(),
            ),
        ],
        properties: json!({
            "class_definitions": model.classes.len(),
            "type_definitions": model.types.len(),
            "property_definitions": model.property_count,
            "extension_property_definitions": model.extension_property_count,
            "interpreted_extension_values": interpreted,
            "max_class_depth": model.max_class_depth,
            "max_type_depth": model.max_type_depth,
            "type_categories": categories,
            "limits": {
                "class_definitions": MAX_CLASS_DEFINITIONS,
                "type_definitions": MAX_TYPE_DEFINITIONS,
                "property_definitions": MAX_PROPERTY_DEFINITIONS,
                "model_depth": MAX_MODEL_DEPTH,
                "fixed_array_elements": MAX_FIXED_ARRAY_ELEMENTS,
            }
        }),
    }
}

fn build_model(
    objects: &[StoredObject],
    streams: &HashMap<PathBuf, Vec<u8>>,
    references: &HashMap<(PathBuf, u16), Vec<PathBuf>>,
    by_path: &HashMap<&Path, &StoredObject>,
    findings: &mut Findings,
) -> Model {
    let mut model = Model::default();
    let dictionaries: Vec<&StoredObject> = objects
        .iter()
        .filter(|object| object.class_id == META_DICTIONARY)
        .collect();
    if dictionaries.len() != 1 {
        findings.push(format!(
            "expected exactly one MetaDictionary, found {}",
            dictionaries.len()
        ));
        return model;
    }
    let dictionary = dictionaries[0];
    let class_paths = references
        .get(&(dictionary.path.clone(), 0x0003))
        .cloned()
        .unwrap_or_default();
    let type_paths = references
        .get(&(dictionary.path.clone(), 0x0004))
        .cloned()
        .unwrap_or_default();
    if class_paths.len() > MAX_CLASS_DEFINITIONS {
        findings.push(format!(
            "MetaDictionary has {} class definitions, limit is {MAX_CLASS_DEFINITIONS}",
            class_paths.len()
        ));
    }
    if type_paths.len() > MAX_TYPE_DEFINITIONS {
        findings.push(format!(
            "MetaDictionary has {} type definitions, limit is {MAX_TYPE_DEFINITIONS}",
            type_paths.len()
        ));
    }

    let mut dynamic_pids = HashMap::<u16, PathBuf>::new();
    for path in class_paths.iter().take(MAX_CLASS_DEFINITIONS) {
        let Some(object) = by_path.get(path.as_path()).copied() else {
            continue;
        };
        if object.class_id != CLASS_DEFINITION {
            findings.push(format!(
                "{} is owned as a ClassDefinition but has class {}",
                path.display(),
                object.class_id
            ));
            continue;
        }
        let Some(id) = required_auid(object, 0x0005, "Identification", findings) else {
            continue;
        };
        let Some(name) = required_string(object, 0x0006, "Name", findings) else {
            continue;
        };
        let parent = property(object, 0x0008)
            .and_then(weak_auid)
            .filter(|parent| parent != &id);
        if property(object, 0x0008).is_none() {
            findings.push(format!("{} omits ParentClass", path.display()));
        }
        let concrete = required_bool(object, 0x000a, "IsConcrete", findings).unwrap_or(false);
        let mut property_defs = Vec::new();
        for property_path in references
            .get(&(object.path.clone(), 0x0009))
            .into_iter()
            .flatten()
        {
            if model.property_count >= MAX_PROPERTY_DEFINITIONS {
                findings.push(format!(
                    "property definition count exceeds {MAX_PROPERTY_DEFINITIONS}"
                ));
                break;
            }
            let Some(property_object) = by_path.get(property_path.as_path()).copied() else {
                continue;
            };
            if property_object.class_id != PROPERTY_DEFINITION {
                findings.push(format!(
                    "{} is owned as a PropertyDefinition but has class {}",
                    property_path.display(),
                    property_object.class_id
                ));
                continue;
            }
            if let Some(definition) = parse_property_definition(property_object, findings) {
                model.property_count += 1;
                if definition.pid >= 0x8000 {
                    model.extension_property_count += 1;
                    if let Some(previous) =
                        dynamic_pids.insert(definition.pid, definition.path.clone())
                    {
                        findings.push(format!(
                            "{} repeats dynamic PID 0x{:04x} from {}",
                            definition.path.display(),
                            definition.pid,
                            previous.display()
                        ));
                    }
                }
                if !model.property_ids.insert(definition.id.clone()) {
                    findings.push(format!(
                        "{} repeats PropertyDefinition Identification {}",
                        definition.path.display(),
                        definition.id
                    ));
                }
                property_defs.push(definition);
            }
        }
        let definition = ClassDef {
            path: object.path.clone(),
            id: id.clone(),
            name,
            parent,
            concrete,
            properties: property_defs,
        };
        if let Some(previous) = model.classes.insert(id.clone(), definition) {
            findings.push(format!(
                "{} repeats ClassDefinition Identification {id} from {}",
                object.path.display(),
                previous.path.display()
            ));
        }
    }

    for path in type_paths.iter().take(MAX_TYPE_DEFINITIONS) {
        let Some(object) = by_path.get(path.as_path()).copied() else {
            continue;
        };
        if let Some(definition) = parse_type_definition(object, streams, findings) {
            let id = definition.id.clone();
            if let Some(previous) = model.types.insert(id.clone(), definition) {
                findings.push(format!(
                    "{} repeats TypeDefinition Identification {id} from {}",
                    object.path.display(),
                    previous.path.display()
                ));
            }
        }
    }
    model
}

fn parse_property_definition(
    object: &StoredObject,
    findings: &mut Findings,
) -> Option<PropertyDef> {
    let id = required_auid(object, 0x0005, "Identification", findings)?;
    let name = required_string(object, 0x0006, "Name", findings)?;
    let type_id = required_auid(object, 0x000b, "Type", findings)?;
    let optional = required_bool(object, 0x000c, "IsOptional", findings)?;
    let pid = required_u16(object, 0x000d, "LocalIdentification", findings)?;
    let unique = optional_bool(object, 0x000e, "IsUniqueIdentifier", findings).unwrap_or(false);
    if pid == 0 {
        findings.push(format!(
            "{} has zero LocalIdentification",
            object.path.display()
        ));
    }
    Some(PropertyDef {
        path: object.path.clone(),
        id,
        name,
        pid,
        type_id,
        optional,
        unique,
    })
}

fn parse_type_definition(
    object: &StoredObject,
    streams: &HashMap<PathBuf, Vec<u8>>,
    findings: &mut Findings,
) -> Option<TypeDef> {
    let id = required_auid(object, 0x0005, "Identification", findings)?;
    let name = required_string(object, 0x0006, "Name", findings)?;
    let kind = match object.class_id.as_str() {
        TYPE_INTEGER => {
            let size = usize::from(required_u8(object, 0x000f, "Size", findings)?);
            let signed = required_bool(object, 0x0010, "IsSigned", findings)?;
            if !matches!(size, 1 | 2 | 4 | 8) {
                findings.push(format!(
                    "{} integer type has unsupported size {size}",
                    object.path.display()
                ));
            }
            TypeKind::Integer { size, signed }
        }
        TYPE_STRONG_REF => TypeKind::StrongRef {
            class_id: required_weak_auid(object, 0x0011, "ReferencedType", findings)?,
        },
        TYPE_WEAK_REF => TypeKind::WeakRef {
            class_id: required_weak_auid(object, 0x0012, "ReferencedType", findings)?,
            target_set: required_auid_array(object, 0x0013, "TargetSet", findings)?,
        },
        TYPE_ENUM => {
            let element_type = required_weak_auid(object, 0x0014, "ElementType", findings)?;
            let names = required_utf16_array(object, 0x0015, "ElementNames", findings)?;
            let values = property(object, 0x0016)
                .map(|property| property.data.as_slice())
                .unwrap_or_default();
            if values.len() != names.len().saturating_mul(8) {
                findings.push(format!(
                    "{} enumeration has {} names but {} value bytes",
                    object.path.display(),
                    names.len(),
                    values.len()
                ));
            }
            let mut unique_names = HashSet::new();
            let mut unique_values = HashSet::new();
            for name in names {
                if name.is_empty() || !unique_names.insert(name) {
                    findings.push(format!(
                        "{} enumeration has an empty or duplicate element name",
                        object.path.display()
                    ));
                }
            }
            for value in values.chunks_exact(8) {
                let number = i64::from_le_bytes(value.try_into().unwrap());
                if !unique_values.insert(number) {
                    findings.push(format!(
                        "{} enumeration repeats element value {number}",
                        object.path.display()
                    ));
                }
            }
            TypeKind::Enumeration {
                element_type,
                values: unique_values,
            }
        }
        TYPE_FIXED_ARRAY => {
            let element_type = required_weak_auid(object, 0x0017, "ElementType", findings)?;
            let count = required_u32(object, 0x0018, "ElementCount", findings)?;
            if count == 0 || count > MAX_FIXED_ARRAY_ELEMENTS {
                findings.push(format!(
                    "{} fixed array count {count} is outside 1..={MAX_FIXED_ARRAY_ELEMENTS}",
                    object.path.display()
                ));
            }
            TypeKind::FixedArray {
                element_type,
                count,
            }
        }
        TYPE_VAR_ARRAY => TypeKind::VariableArray {
            element_type: required_weak_auid(object, 0x0019, "ElementType", findings)?,
        },
        TYPE_SET => TypeKind::Set {
            element_type: required_weak_auid(object, 0x001a, "ElementType", findings)?,
        },
        TYPE_STRING => TypeKind::String {
            element_type: required_weak_auid(object, 0x001b, "ElementType", findings)?,
        },
        TYPE_STREAM => TypeKind::Stream,
        TYPE_RECORD => {
            let member_types =
                required_weak_auid_vector(object, 0x001c, "MemberTypes", streams, findings)?;
            let names = required_utf16_array(object, 0x001d, "MemberNames", findings)?;
            if member_types.is_empty() || member_types.len() != names.len() {
                findings.push(format!(
                    "{} record has {} member types and {} member names",
                    object.path.display(),
                    member_types.len(),
                    names.len()
                ));
            }
            if names.iter().any(String::is_empty)
                || names.iter().collect::<HashSet<_>>().len() != names.len()
            {
                findings.push(format!(
                    "{} record has empty or duplicate member names",
                    object.path.display()
                ));
            }
            TypeKind::Record { member_types }
        }
        TYPE_RENAME => TypeKind::Rename {
            renamed_type: required_weak_auid(object, 0x001e, "RenamedType", findings)?,
        },
        TYPE_EXT_ENUM => {
            let names = required_utf16_array(object, 0x001f, "ElementNames", findings)?;
            let raw_values = property(object, 0x0020)
                .map(|property| property.data.as_slice())
                .unwrap_or_default();
            if raw_values.len() != names.len().saturating_mul(16) {
                findings.push(format!(
                    "{} extendible enumeration has {} names but {} value bytes",
                    object.path.display(),
                    names.len(),
                    raw_values.len()
                ));
            }
            let values: HashSet<Vec<u8>> =
                raw_values.chunks_exact(16).map(<[u8]>::to_vec).collect();
            if values.len() != names.len()
                || names.iter().any(String::is_empty)
                || names.iter().collect::<HashSet<_>>().len() != names.len()
            {
                findings.push(format!(
                    "{} extendible enumeration has empty or duplicate elements",
                    object.path.display()
                ));
            }
            TypeKind::ExtendibleEnumeration { values }
        }
        TYPE_INDIRECT => TypeKind::Indirect,
        TYPE_OPAQUE => TypeKind::Opaque,
        TYPE_CHARACTER => TypeKind::Character { size: 2 },
        TYPE_GENERIC_CHARACTER => {
            let size_property = object
                .properties
                .iter()
                .find(|property| !matches!(property.pid, 0x0005..=0x0007));
            let size = size_property
                .and_then(|property| property.data.first().copied())
                .map(usize::from)
                .unwrap_or_default();
            if !matches!(size, 1 | 2 | 4) {
                findings.push(format!(
                    "{} generic character has unsupported size {size}",
                    object.path.display()
                ));
            }
            TypeKind::Character { size }
        }
        other => {
            findings.push(format!(
                "{} has unsupported TypeDefinition class {other}",
                object.path.display()
            ));
            return None;
        }
    };
    Some(TypeDef {
        path: object.path.clone(),
        id,
        name,
        kind,
    })
}

fn validate_model(model: &mut Model, findings: &mut Findings) {
    for class in model.classes.values() {
        if let Some(parent) = &class.parent {
            if parent != ROOT_CLASS && !model.classes.contains_key(parent) {
                findings.push(format!(
                    "{} class {} references missing parent {parent}",
                    class.path.display(),
                    class.name
                ));
            }
        }
        let mut local_pids = HashSet::new();
        let mut local_names = HashSet::new();
        for definition in &class.properties {
            if !model.types.contains_key(&definition.type_id)
                && !is_baseline_type_id(&definition.type_id)
            {
                findings.push(format!(
                    "{} property {} references missing type {}",
                    definition.path.display(),
                    definition.name,
                    definition.type_id
                ));
            }
            if !local_pids.insert(definition.pid) {
                findings.push(format!(
                    "{} class {} repeats local PID 0x{:04x}",
                    class.path.display(),
                    class.name,
                    definition.pid
                ));
            }
            if !local_names.insert(&definition.name) {
                findings.push(format!(
                    "{} class {} repeats property name {}",
                    class.path.display(),
                    class.name,
                    definition.name
                ));
            }
            if definition.unique && definition.optional {
                findings.push(format!(
                    "{} unique property {} is optional",
                    definition.path.display(),
                    definition.name
                ));
            }
        }
    }

    let class_ids: Vec<String> = model.classes.keys().cloned().collect();
    for class_id in class_ids {
        let mut seen = HashSet::new();
        let mut pids = HashMap::<u16, &str>::new();
        let mut current = Some(class_id.as_str());
        let mut depth = 0;
        while let Some(id) = current {
            depth += 1;
            if depth > MAX_MODEL_DEPTH {
                findings.push(format!(
                    "class inheritance from {class_id} exceeds depth {MAX_MODEL_DEPTH}"
                ));
                break;
            }
            if !seen.insert(id.to_owned()) {
                findings.push(format!("class inheritance cycle includes {id}"));
                break;
            }
            let Some(class) = model.classes.get(id) else {
                break;
            };
            for definition in &class.properties {
                if let Some(previous) = pids.insert(definition.pid, &definition.name) {
                    findings.push(format!(
                        "class {} property {} reuses inherited PID 0x{:04x} from {previous}",
                        model
                            .classes
                            .get(&class_id)
                            .map(|class| class.name.as_str())
                            .unwrap_or(&class_id),
                        definition.name,
                        definition.pid
                    ));
                }
            }
            current = class
                .parent
                .as_deref()
                .filter(|parent| *parent != ROOT_CLASS);
        }
        model.max_class_depth = model.max_class_depth.max(depth.min(MAX_MODEL_DEPTH));
    }

    let type_ids: Vec<String> = model.types.keys().cloned().collect();
    for type_id in type_ids {
        let mut stack = Vec::new();
        match storage_for(&type_id, model, &mut stack, 0) {
            Ok((_, depth)) => model.max_type_depth = model.max_type_depth.max(depth),
            Err(error) => findings.push(error),
        }
    }

    for typedef in model.types.values() {
        match &typedef.kind {
            TypeKind::StrongRef { class_id } => {
                if class_id != ROOT_CLASS && !model.classes.contains_key(class_id) {
                    findings.push(format!(
                        "{} type {} references missing class {class_id}",
                        typedef.path.display(),
                        typedef.name
                    ));
                }
            }
            TypeKind::WeakRef {
                class_id,
                target_set,
            } => {
                if class_id != ROOT_CLASS && !model.classes.contains_key(class_id) {
                    findings.push(format!(
                        "{} type {} references missing class {class_id}",
                        typedef.path.display(),
                        typedef.name
                    ));
                }
                for property_id in target_set {
                    if !model.property_ids.contains(property_id)
                        && !is_baseline_property_id(property_id)
                    {
                        findings.push(format!(
                            "{} weak type {} has unresolved TargetSet property {property_id}",
                            typedef.path.display(),
                            typedef.name
                        ));
                    }
                }
            }
            TypeKind::Enumeration { element_type, .. } => {
                if !matches!(
                    model.types.get(element_type).map(|typedef| &typedef.kind),
                    Some(TypeKind::Integer { .. })
                ) && baseline_integer(element_type).is_none()
                {
                    findings.push(format!(
                        "{} enumeration {} does not use an integer element type",
                        typedef.path.display(),
                        typedef.name
                    ));
                }
            }
            TypeKind::String { element_type }
                if !matches!(
                    model.types.get(element_type).map(|typedef| &typedef.kind),
                    Some(TypeKind::Character { .. })
                ) && baseline_character_size(element_type).is_none() =>
            {
                findings.push(format!(
                    "{} string {} does not use a character element type",
                    typedef.path.display(),
                    typedef.name
                ));
            }
            _ => {}
        }
    }
}

fn validate_extension_values(
    objects: &[StoredObject],
    stream_paths: &HashSet<PathBuf>,
    references: &HashMap<(PathBuf, u16), Vec<PathBuf>>,
    by_path: &HashMap<&Path, &StoredObject>,
    model: &Model,
    findings: &mut Findings,
) -> usize {
    let mut interpreted = 0;
    for object in objects.iter().filter(|object| {
        object.path != Path::new("/")
            && !is_meta_definition_class(&object.class_id)
            && object.class_id != META_DICTIONARY
    }) {
        let Some(class) = model.classes.get(&object.class_id) else {
            if !is_standard_instance_class(&object.class_id) {
                findings.push(format!(
                    "{} uses extension class {} without a ClassDefinition",
                    object.path.display(),
                    object.class_id
                ));
            }
            continue;
        };
        let extension_class = !is_standard_instance_class(&object.class_id);
        let Some(definitions) = inherited_properties(class, model, findings) else {
            continue;
        };
        let by_pid: HashMap<u16, &PropertyDef> = definitions
            .iter()
            .map(|definition| (definition.pid, *definition))
            .collect();
        for definition in definitions {
            if (extension_class || definition.pid >= 0x8000)
                && !definition.optional
                && property(object, definition.pid).is_none()
            {
                findings.push(format!(
                    "{} extension property {} (PID 0x{:04x}) is required but missing",
                    object.path.display(),
                    definition.name,
                    definition.pid
                ));
            }
        }
        for value in &object.properties {
            if !extension_class && value.pid < 0x8000 {
                continue;
            }
            let Some(definition) = by_pid.get(&value.pid).copied() else {
                findings.push(format!(
                    "{} extension PID 0x{:04x} has no PropertyDefinition",
                    object.path.display(),
                    value.pid
                ));
                continue;
            };
            interpreted += 1;
            if let Err(error) = validate_value(
                object,
                value,
                definition,
                stream_paths,
                references,
                by_path,
                model,
            ) {
                findings.push(error);
            }
        }
        if extension_class && !class.concrete {
            findings.push(format!(
                "{} instantiates abstract extension class {}",
                object.path.display(),
                class.name
            ));
        }
    }
    interpreted
}

fn inherited_properties<'a>(
    class: &'a ClassDef,
    model: &'a Model,
    findings: &mut Findings,
) -> Option<Vec<&'a PropertyDef>> {
    let mut result = Vec::new();
    let mut current = Some(class);
    let mut seen = HashSet::new();
    for _ in 0..MAX_MODEL_DEPTH {
        let Some(definition) = current else {
            return Some(result);
        };
        if !seen.insert(definition.id.as_str()) {
            return None;
        }
        result.extend(&definition.properties);
        current = definition
            .parent
            .as_ref()
            .filter(|parent| parent.as_str() != ROOT_CLASS)
            .and_then(|parent| model.classes.get(parent));
    }
    findings.push(format!(
        "class {} inheritance exceeds depth {MAX_MODEL_DEPTH}",
        class.name
    ));
    None
}

fn validate_value(
    object: &StoredObject,
    value: &StoredProperty,
    definition: &PropertyDef,
    stream_paths: &HashSet<PathBuf>,
    references: &HashMap<(PathBuf, u16), Vec<PathBuf>>,
    by_path: &HashMap<&Path, &StoredObject>,
    model: &Model,
) -> Result<(), String> {
    let (storage, _) = storage_for(&definition.type_id, model, &mut Vec::new(), 0)
        .map_err(|error| format!("{}: {error}", object.path.display()))?;
    if !format_matches(storage, value.format) {
        return Err(format!(
            "{} extension property {} (PID 0x{:04x}) uses stored format 0x{:02x}, expected {}",
            object.path.display(),
            definition.name,
            definition.pid,
            value.format,
            storage_name(storage)
        ));
    }
    match storage {
        Storage::Data(_) => {
            validate_data(&definition.type_id, &value.data, model, 0).map_err(|error| {
                format!(
                    "{} extension property {} (PID 0x{:04x}): {error}",
                    object.path.display(),
                    definition.name,
                    definition.pid
                )
            })
        }
        Storage::Stream => {
            let name = value
                .data
                .strip_prefix(&[0x55])
                .and_then(utf16_string)
                .filter(|name| safe_child_name(name))
                .ok_or_else(|| {
                    format!(
                        "{} extension stream property {} has an invalid stream name",
                        object.path.display(),
                        definition.name
                    )
                })?;
            if !stream_paths.contains(&object.path.join(&name)) {
                return Err(format!(
                    "{} extension stream property {} references missing stream {name:?}",
                    object.path.display(),
                    definition.name
                ));
            }
            Ok(())
        }
        Storage::StrongRef | Storage::StrongVector | Storage::StrongSet => {
            let target_class = referenced_class(&definition.type_id, model).ok_or_else(|| {
                format!(
                    "{} extension property {} has no referenced class",
                    object.path.display(),
                    definition.name
                )
            })?;
            for path in references
                .get(&(object.path.clone(), definition.pid))
                .into_iter()
                .flatten()
            {
                let target = by_path.get(path.as_path()).copied().ok_or_else(|| {
                    format!(
                        "{} extension property {} references missing {}",
                        object.path.display(),
                        definition.name,
                        path.display()
                    )
                })?;
                if !class_is_a(&target.class_id, target_class, model) {
                    return Err(format!(
                        "{} extension property {} targets class {}, expected {target_class}",
                        object.path.display(),
                        definition.name,
                        target.class_id
                    ));
                }
            }
            Ok(())
        }
        Storage::WeakRef | Storage::WeakVector | Storage::WeakSet => Ok(()),
    }
}

fn validate_data(type_id: &str, data: &[u8], model: &Model, depth: usize) -> Result<(), String> {
    if depth > MAX_MODEL_DEPTH {
        return Err(format!("value type nesting exceeds {MAX_MODEL_DEPTH}"));
    }
    let Some(typedef) = model.types.get(type_id) else {
        if let Some((size, _)) = baseline_integer(type_id)
            .or_else(|| baseline_character_size(type_id).map(|size| (size, false)))
        {
            return if data.len() == size {
                Ok(())
            } else {
                Err(format!("expected {size} data bytes, found {}", data.len()))
            };
        }
        return Err(format!("unknown type {type_id}"));
    };
    match &typedef.kind {
        TypeKind::Integer { size, .. } | TypeKind::Character { size } => {
            if data.len() == *size {
                Ok(())
            } else {
                Err(format!("expected {size} data bytes, found {}", data.len()))
            }
        }
        TypeKind::Enumeration {
            element_type,
            values,
        } => {
            validate_data(element_type, data, model, depth + 1)?;
            let integer = integer_value(element_type, data, model)
                .ok_or_else(|| "cannot decode enumeration integer".to_owned())?;
            if values.contains(&integer) {
                Ok(())
            } else {
                Err(format!("enumeration value {integer} is not declared"))
            }
        }
        TypeKind::FixedArray {
            element_type,
            count,
        } => {
            let size = fixed_data_size(element_type, model, depth + 1)?;
            let expected = size
                .checked_mul(usize::try_from(*count).map_err(|_| "array count overflow")?)
                .ok_or_else(|| "fixed-array byte length overflow".to_owned())?;
            if data.len() != expected {
                return Err(format!(
                    "fixed array expects {expected} bytes, found {}",
                    data.len()
                ));
            }
            for chunk in data.chunks_exact(size) {
                validate_data(element_type, chunk, model, depth + 1)?;
            }
            Ok(())
        }
        TypeKind::VariableArray { element_type } | TypeKind::Set { element_type } => {
            let size = fixed_data_size(element_type, model, depth + 1)?;
            if size == 0 || !data.len().is_multiple_of(size) {
                return Err(format!(
                    "array byte length {} is not a multiple of element size {size}",
                    data.len()
                ));
            }
            let mut unique = HashSet::new();
            for chunk in data.chunks_exact(size) {
                validate_data(element_type, chunk, model, depth + 1)?;
                if matches!(&typedef.kind, TypeKind::Set { .. }) && !unique.insert(chunk) {
                    return Err("set contains a duplicate value".to_owned());
                }
            }
            Ok(())
        }
        TypeKind::String { element_type } => {
            let size = fixed_data_size(element_type, model, depth + 1)?;
            if size == 0
                || data.len() < size
                || !data.len().is_multiple_of(size)
                || !data[data.len() - size..].iter().all(|byte| *byte == 0)
            {
                return Err(format!(
                    "string is not terminated in {size}-byte character units"
                ));
            }
            if size == 2 && utf16_string(data).is_none() {
                return Err("string is not valid UTF-16LE".to_owned());
            }
            Ok(())
        }
        TypeKind::Record { member_types } => {
            let mut offset = 0usize;
            for member in member_types {
                let size = fixed_data_size(member, model, depth + 1)?;
                let end = offset
                    .checked_add(size)
                    .ok_or_else(|| "record byte length overflow".to_owned())?;
                let bytes = data
                    .get(offset..end)
                    .ok_or_else(|| "record value is truncated".to_owned())?;
                validate_data(member, bytes, model, depth + 1)?;
                offset = end;
            }
            if offset == data.len() {
                Ok(())
            } else {
                Err(format!(
                    "record expects {offset} bytes, found {}",
                    data.len()
                ))
            }
        }
        TypeKind::Rename { renamed_type } => validate_data(renamed_type, data, model, depth + 1),
        TypeKind::ExtendibleEnumeration { values } => {
            if data.len() != 16 {
                Err(format!(
                    "extendible enumeration expects 16 bytes, found {}",
                    data.len()
                ))
            } else if !values.contains(data) {
                Err("extendible enumeration value is not declared".to_owned())
            } else {
                Ok(())
            }
        }
        TypeKind::Indirect | TypeKind::Opaque => {
            if data.len() < 17 || data[0] != b'L' {
                return Err("indirect/opaque value lacks a little-endian type header".to_owned());
            }
            let actual_type = auid_string(&data[1..17])
                .ok_or_else(|| "indirect/opaque type identifier is malformed".to_owned())?;
            if !model.types.contains_key(&actual_type) && !is_baseline_type_id(&actual_type) {
                return Err(format!(
                    "indirect/opaque value references unknown type {actual_type}"
                ));
            }
            if matches!(&typedef.kind, TypeKind::Indirect) {
                validate_data(&actual_type, &data[17..], model, depth + 1)
            } else {
                Ok(())
            }
        }
        TypeKind::StrongRef { .. } | TypeKind::WeakRef { .. } | TypeKind::Stream => {
            Err("reference/stream type cannot use inline data".to_owned())
        }
    }
}

fn storage_for(
    type_id: &str,
    model: &Model,
    stack: &mut Vec<String>,
    depth: usize,
) -> Result<(Storage, usize), String> {
    if depth > MAX_MODEL_DEPTH {
        return Err(format!(
            "type graph from {type_id} exceeds depth {MAX_MODEL_DEPTH}"
        ));
    }
    if stack.iter().any(|value| value == type_id) {
        return Err(format!("type definition cycle includes {type_id}"));
    }
    let Some(typedef) = model.types.get(type_id) else {
        if let Some((size, _)) = baseline_integer(type_id)
            .or_else(|| baseline_character_size(type_id).map(|size| (size, false)))
        {
            return Ok((Storage::Data(Some(size)), depth + 1));
        }
        return Err(format!("unresolved type definition {type_id}"));
    };
    stack.push(type_id.to_owned());
    let result = match &typedef.kind {
        TypeKind::Integer { size, .. } | TypeKind::Character { size } => {
            Ok((Storage::Data(Some(*size)), depth + 1))
        }
        TypeKind::StrongRef { .. } => Ok((Storage::StrongRef, depth + 1)),
        TypeKind::WeakRef { .. } => Ok((Storage::WeakRef, depth + 1)),
        TypeKind::Enumeration { element_type, .. } => {
            let (storage, child_depth) = storage_for(element_type, model, stack, depth + 1)?;
            if let Storage::Data(Some(size)) = storage {
                Ok((Storage::Data(Some(size)), child_depth))
            } else {
                Err(format!(
                    "enumeration type {} has non-integer storage",
                    typedef.name
                ))
            }
        }
        TypeKind::FixedArray {
            element_type,
            count,
        } => {
            let (storage, child_depth) = storage_for(element_type, model, stack, depth + 1)?;
            if let Storage::Data(Some(size)) = storage {
                let bytes = size
                    .checked_mul(usize::try_from(*count).map_err(|_| "fixed-array count overflow")?)
                    .ok_or_else(|| "fixed-array byte length overflow".to_owned())?;
                Ok((Storage::Data(Some(bytes)), child_depth))
            } else {
                Err(format!(
                    "fixed array type {} has non-fixed-data elements",
                    typedef.name
                ))
            }
        }
        TypeKind::VariableArray { element_type } => {
            let (storage, child_depth) = storage_for(element_type, model, stack, depth + 1)?;
            match storage {
                Storage::StrongRef => Ok((Storage::StrongVector, child_depth)),
                Storage::WeakRef => Ok((Storage::WeakVector, child_depth)),
                Storage::Data(Some(_)) => Ok((Storage::Data(None), child_depth)),
                _ => Err(format!(
                    "variable array type {} has unsupported element storage",
                    typedef.name
                )),
            }
        }
        TypeKind::Set { element_type } => {
            let (storage, child_depth) = storage_for(element_type, model, stack, depth + 1)?;
            match storage {
                Storage::StrongRef => Ok((Storage::StrongSet, child_depth)),
                Storage::WeakRef => Ok((Storage::WeakSet, child_depth)),
                Storage::Data(Some(_)) => Ok((Storage::Data(None), child_depth)),
                _ => Err(format!(
                    "set type {} has unsupported element storage",
                    typedef.name
                )),
            }
        }
        TypeKind::String { element_type } => {
            let (storage, child_depth) = storage_for(element_type, model, stack, depth + 1)?;
            if matches!(storage, Storage::Data(Some(1 | 2 | 4))) {
                Ok((Storage::Data(None), child_depth))
            } else {
                Err(format!(
                    "string type {} has invalid character storage",
                    typedef.name
                ))
            }
        }
        TypeKind::Stream => Ok((Storage::Stream, depth + 1)),
        TypeKind::Record { member_types } => {
            let mut size = 0usize;
            let mut max_depth = depth + 1;
            for member in member_types {
                let (storage, child_depth) = storage_for(member, model, stack, depth + 1)?;
                let Storage::Data(Some(member_size)) = storage else {
                    return Err(format!(
                        "record type {} has a non-fixed-data member",
                        typedef.name
                    ));
                };
                size = size
                    .checked_add(member_size)
                    .ok_or_else(|| "record byte length overflow".to_owned())?;
                max_depth = max_depth.max(child_depth);
            }
            Ok((Storage::Data(Some(size)), max_depth))
        }
        TypeKind::Rename { renamed_type } => storage_for(renamed_type, model, stack, depth + 1),
        TypeKind::ExtendibleEnumeration { .. } => Ok((Storage::Data(Some(16)), depth + 1)),
        TypeKind::Indirect | TypeKind::Opaque => Ok((Storage::Data(None), depth + 1)),
    };
    stack.pop();
    result
}

fn fixed_data_size(type_id: &str, model: &Model, depth: usize) -> Result<usize, String> {
    let (storage, _) = storage_for(type_id, model, &mut Vec::new(), depth)?;
    match storage {
        Storage::Data(Some(size)) if size != 0 => Ok(size),
        _ => Err(format!("type {type_id} does not have a fixed inline size")),
    }
}

fn integer_value(type_id: &str, data: &[u8], model: &Model) -> Option<i64> {
    let (size, signed) = match model.types.get(type_id).map(|value| &value.kind) {
        Some(TypeKind::Integer { size, signed }) => (*size, *signed),
        Some(_) => return None,
        None => baseline_integer(type_id)?,
    };
    if data.len() != size {
        return None;
    }
    let mut bytes = [0u8; 8];
    bytes[..size].copy_from_slice(data);
    if signed && data[size - 1] & 0x80 != 0 {
        bytes[size..].fill(0xff);
    }
    Some(i64::from_le_bytes(bytes))
}

fn referenced_class<'a>(type_id: &str, model: &'a Model) -> Option<&'a str> {
    let typedef = model.types.get(type_id)?;
    match &typedef.kind {
        TypeKind::StrongRef { class_id } | TypeKind::WeakRef { class_id, .. } => Some(class_id),
        TypeKind::VariableArray { element_type } | TypeKind::Set { element_type } => {
            referenced_class(element_type, model)
        }
        TypeKind::Rename { renamed_type } => referenced_class(renamed_type, model),
        _ => None,
    }
}

fn class_is_a(actual: &str, expected: &str, model: &Model) -> bool {
    if expected == ROOT_CLASS || actual == expected {
        return true;
    }
    let mut current = model.classes.get(actual);
    let mut seen = HashSet::new();
    for _ in 0..MAX_MODEL_DEPTH {
        let Some(class) = current else {
            return false;
        };
        if !seen.insert(class.id.as_str()) {
            return false;
        }
        let Some(parent) = class.parent.as_deref() else {
            return false;
        };
        if parent == expected || parent == ROOT_CLASS && expected == ROOT_CLASS {
            return true;
        }
        current = model.classes.get(parent);
    }
    false
}

fn format_matches(storage: Storage, format: u16) -> bool {
    match storage {
        Storage::Data(_) => format == SF_DATA,
        Storage::Stream => format == SF_DATA_STREAM,
        Storage::StrongRef => format == SF_STRONG_REF,
        Storage::WeakRef => format == SF_WEAK_REF,
        Storage::StrongVector => format == SF_STRONG_VECTOR,
        Storage::WeakVector => format == SF_WEAK_VECTOR,
        // The AAF Object Manager permits set properties to use vector storage
        // when the set stored form is unavailable.
        Storage::StrongSet => matches!(format, SF_STRONG_SET | SF_STRONG_VECTOR),
        Storage::WeakSet => matches!(format, SF_WEAK_SET | SF_WEAK_VECTOR),
    }
}

fn storage_name(storage: Storage) -> &'static str {
    match storage {
        Storage::Data(_) => "inline data",
        Storage::Stream => "data stream",
        Storage::StrongRef => "strong reference",
        Storage::WeakRef => "weak reference",
        Storage::StrongVector => "strong-reference vector",
        Storage::WeakVector => "weak-reference vector",
        Storage::StrongSet => "strong-reference set/vector",
        Storage::WeakSet => "weak-reference set/vector",
    }
}

fn type_category(kind: &TypeKind) -> &'static str {
    match kind {
        TypeKind::Integer { .. } => "integer",
        TypeKind::StrongRef { .. } => "strong_reference",
        TypeKind::WeakRef { .. } => "weak_reference",
        TypeKind::Enumeration { .. } => "enumeration",
        TypeKind::FixedArray { .. } => "fixed_array",
        TypeKind::VariableArray { .. } => "variable_array",
        TypeKind::Set { .. } => "set",
        TypeKind::String { .. } => "string",
        TypeKind::Stream => "stream",
        TypeKind::Record { .. } => "record",
        TypeKind::Rename { .. } => "rename",
        TypeKind::ExtendibleEnumeration { .. } => "extendible_enumeration",
        TypeKind::Indirect => "indirect",
        TypeKind::Opaque => "opaque",
        TypeKind::Character { .. } => "character",
    }
}

fn property(object: &StoredObject, pid: u16) -> Option<&StoredProperty> {
    object
        .properties
        .iter()
        .find(|property| property.pid == pid)
}

fn required_auid(
    object: &StoredObject,
    pid: u16,
    name: &str,
    findings: &mut Findings,
) -> Option<String> {
    let result = property(object, pid).and_then(|property| auid_string(&property.data));
    if result.is_none() {
        findings.push(format!(
            "{} has missing or malformed {name} (PID 0x{pid:04x})",
            object.path.display()
        ));
    }
    result
}

fn required_weak_auid(
    object: &StoredObject,
    pid: u16,
    name: &str,
    findings: &mut Findings,
) -> Option<String> {
    let result = property(object, pid).and_then(weak_auid);
    if result.is_none() {
        findings.push(format!(
            "{} has missing or malformed {name} weak reference (PID 0x{pid:04x})",
            object.path.display()
        ));
    }
    result
}

fn required_string(
    object: &StoredObject,
    pid: u16,
    name: &str,
    findings: &mut Findings,
) -> Option<String> {
    let result = property(object, pid)
        .and_then(|property| utf16_string(&property.data))
        .filter(|value| !value.is_empty());
    if result.is_none() {
        findings.push(format!(
            "{} has missing or malformed {name} (PID 0x{pid:04x})",
            object.path.display()
        ));
    }
    result
}

fn required_bool(
    object: &StoredObject,
    pid: u16,
    name: &str,
    findings: &mut Findings,
) -> Option<bool> {
    let result = optional_bool(object, pid, name, findings);
    if property(object, pid).is_none() {
        findings.push(format!(
            "{} omits {name} (PID 0x{pid:04x})",
            object.path.display()
        ));
    }
    result
}

fn optional_bool(
    object: &StoredObject,
    pid: u16,
    name: &str,
    findings: &mut Findings,
) -> Option<bool> {
    let property = property(object, pid)?;
    match property.data.as_slice() {
        [0] => Some(false),
        [1] => Some(true),
        _ => {
            findings.push(format!(
                "{} has malformed {name} (PID 0x{pid:04x})",
                object.path.display()
            ));
            None
        }
    }
}

fn required_u8(object: &StoredObject, pid: u16, name: &str, findings: &mut Findings) -> Option<u8> {
    let result = property(object, pid)
        .and_then(|property| (property.data.len() == 1).then_some(property.data[0]));
    if result.is_none() {
        findings.push(format!(
            "{} has missing or malformed {name} (PID 0x{pid:04x})",
            object.path.display()
        ));
    }
    result
}

fn required_u16(
    object: &StoredObject,
    pid: u16,
    name: &str,
    findings: &mut Findings,
) -> Option<u16> {
    let result = property(object, pid).and_then(|property| {
        Some(u16::from_le_bytes(
            property.data.as_slice().try_into().ok()?,
        ))
    });
    if result.is_none() {
        findings.push(format!(
            "{} has missing or malformed {name} (PID 0x{pid:04x})",
            object.path.display()
        ));
    }
    result
}

fn required_u32(
    object: &StoredObject,
    pid: u16,
    name: &str,
    findings: &mut Findings,
) -> Option<u32> {
    let result = property(object, pid).and_then(|property| {
        Some(u32::from_le_bytes(
            property.data.as_slice().try_into().ok()?,
        ))
    });
    if result.is_none() {
        findings.push(format!(
            "{} has missing or malformed {name} (PID 0x{pid:04x})",
            object.path.display()
        ));
    }
    result
}

fn required_auid_array(
    object: &StoredObject,
    pid: u16,
    name: &str,
    findings: &mut Findings,
) -> Option<Vec<String>> {
    let property = property(object, pid);
    let result = property.and_then(|property| auid_array(&property.data));
    if result.is_none() {
        findings.push(format!(
            "{} has missing or malformed {name} (PID 0x{pid:04x})",
            object.path.display()
        ));
    }
    result
}

fn required_utf16_array(
    object: &StoredObject,
    pid: u16,
    name: &str,
    findings: &mut Findings,
) -> Option<Vec<String>> {
    let result = property(object, pid).and_then(|property| utf16_array(&property.data));
    if result.is_none() {
        findings.push(format!(
            "{} has missing or malformed {name} (PID 0x{pid:04x})",
            object.path.display()
        ));
    }
    result
}

fn required_weak_auid_vector(
    object: &StoredObject,
    pid: u16,
    name: &str,
    streams: &HashMap<PathBuf, Vec<u8>>,
    findings: &mut Findings,
) -> Option<Vec<String>> {
    let result = property(object, pid).and_then(|property| {
        if !matches!(property.format, SF_WEAK_VECTOR | SF_WEAK_SET) {
            return None;
        }
        let index_name = utf16_string(&property.data)?;
        let bytes = streams.get(&object.path.join(format!("{index_name} index")))?;
        weak_index_auids(bytes)
    });
    if result.is_none() {
        findings.push(format!(
            "{} has missing or malformed {name} weak vector (PID 0x{pid:04x})",
            object.path.display()
        ));
    }
    result
}

fn weak_auid(property: &StoredProperty) -> Option<String> {
    if property.format != SF_WEAK_REF || property.data.len() < 5 {
        return None;
    }
    let key_pid = u16::from_le_bytes(property.data[2..4].try_into().ok()?);
    let key_size = usize::from(property.data[4]);
    if key_pid != 0x0005 || key_size != 16 || property.data.len() != 21 {
        return None;
    }
    auid_string(&property.data[5..])
}

fn weak_index_auids(bytes: &[u8]) -> Option<Vec<String>> {
    if bytes.len() < 9 {
        return None;
    }
    let count = usize::try_from(u32::from_le_bytes(bytes[0..4].try_into().ok()?)).ok()?;
    let key_pid = u16::from_le_bytes(bytes[6..8].try_into().ok()?);
    let key_size = usize::from(bytes[8]);
    if key_pid != 0x0005 || key_size != 16 {
        return None;
    }
    let expected = count.checked_mul(16)?.checked_add(9)?;
    if bytes.len() != expected {
        return None;
    }
    bytes[9..].chunks_exact(16).map(auid_string).collect()
}

fn auid_array(bytes: &[u8]) -> Option<Vec<String>> {
    if !bytes.len().is_multiple_of(16) {
        return None;
    }
    bytes.chunks_exact(16).map(auid_string).collect()
}

fn auid_string(bytes: &[u8]) -> Option<String> {
    let value: [u8; 16] = bytes.try_into().ok()?;
    Some(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        value[3],
        value[2],
        value[1],
        value[0],
        value[5],
        value[4],
        value[7],
        value[6],
        value[8],
        value[9],
        value[10],
        value[11],
        value[12],
        value[13],
        value[14],
        value[15]
    ))
}

fn utf16_string(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 || !bytes.len().is_multiple_of(2) || !bytes.ends_with(&[0, 0]) {
        return None;
    }
    let units: Vec<u16> = bytes[..bytes.len() - 2]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    if units.contains(&0) {
        return None;
    }
    String::from_utf16(&units).ok()
}

fn utf16_array(bytes: &[u8]) -> Option<Vec<String>> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    if bytes.is_empty() {
        return Some(Vec::new());
    }
    if !bytes.ends_with(&[0, 0]) {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let mut result = Vec::new();
    let mut start = 0;
    for (index, unit) in units.iter().enumerate() {
        if *unit == 0 {
            result.push(String::from_utf16(&units[start..index]).ok()?);
            start = index + 1;
        }
    }
    (start == units.len()).then_some(result)
}

fn safe_child_name(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !value.chars().any(char::is_control)
}

fn is_meta_definition_class(class_id: &str) -> bool {
    matches!(
        class_id,
        CLASS_DEFINITION
            | PROPERTY_DEFINITION
            | TYPE_INTEGER
            | TYPE_STRONG_REF
            | TYPE_WEAK_REF
            | TYPE_ENUM
            | TYPE_FIXED_ARRAY
            | TYPE_VAR_ARRAY
            | TYPE_SET
            | TYPE_STRING
            | TYPE_STREAM
            | TYPE_RECORD
            | TYPE_RENAME
            | TYPE_EXT_ENUM
            | TYPE_INDIRECT
            | TYPE_OPAQUE
            | TYPE_CHARACTER
            | TYPE_GENERIC_CHARACTER
    )
}

fn is_standard_instance_class(class_id: &str) -> bool {
    const SUFFIX: &str = "-060e-2b3402060101";
    class_id.ends_with(SUFFIX)
        && (class_id.starts_with("0d010101-0101-")
            || class_id.starts_with("0d010400-")
            || class_id.starts_with("0d010401-"))
}

fn is_baseline_type_id(type_id: &str) -> bool {
    type_id.ends_with("-060e-2b3401040101")
}

fn baseline_integer(type_id: &str) -> Option<(usize, bool)> {
    match type_id {
        "01010100-0000-0000-060e-2b3401040101" => Some((1, false)),
        "01010200-0000-0000-060e-2b3401040101" => Some((2, false)),
        "01010300-0000-0000-060e-2b3401040101" => Some((4, false)),
        "01010400-0000-0000-060e-2b3401040101" => Some((8, false)),
        "01010500-0000-0000-060e-2b3401040101" => Some((1, true)),
        "01010600-0000-0000-060e-2b3401040101" => Some((2, true)),
        "01010700-0000-0000-060e-2b3401040101" => Some((4, true)),
        "01010800-0000-0000-060e-2b3401040101" => Some((8, true)),
        _ => None,
    }
}

fn baseline_character_size(type_id: &str) -> Option<usize> {
    match type_id {
        "01100100-0000-0000-060e-2b3401040101" => Some(2),
        "01100300-0000-0000-060e-2b3401040101" => Some(1),
        _ => None,
    }
}

fn is_baseline_property_id(property_id: &str) -> bool {
    property_id.ends_with("-060e-2b3401010102") || property_id.ends_with("-060e-2b3401010101")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_bytes(value: u128) -> Vec<u8> {
        let bytes = value.to_be_bytes();
        [
            &[bytes[3], bytes[2], bytes[1], bytes[0]][..],
            &[bytes[5], bytes[4]][..],
            &[bytes[7], bytes[6]][..],
            &bytes[8..],
        ]
        .concat()
    }

    #[test]
    fn parses_utf16_name_arrays() {
        let bytes: Vec<u8> = ["one", "two"]
            .into_iter()
            .flat_map(|value| value.encode_utf16().chain([0]))
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(utf16_array(&bytes).unwrap(), ["one", "two"]);
        assert!(utf16_array(&bytes[..bytes.len() - 1]).is_none());
    }

    #[test]
    fn validates_signed_enumerations() {
        let integer_id = "00000000-0000-0000-0000-000000000001".to_owned();
        let enum_id = "00000000-0000-0000-0000-000000000002".to_owned();
        let mut model = Model::default();
        model.types.insert(
            integer_id.clone(),
            TypeDef {
                path: PathBuf::from("/int"),
                id: integer_id.clone(),
                name: "Int16".to_owned(),
                kind: TypeKind::Integer {
                    size: 2,
                    signed: true,
                },
            },
        );
        model.types.insert(
            enum_id.clone(),
            TypeDef {
                path: PathBuf::from("/enum"),
                id: enum_id.clone(),
                name: "SignedEnum".to_owned(),
                kind: TypeKind::Enumeration {
                    element_type: integer_id,
                    values: HashSet::from([-1, 2]),
                },
            },
        );
        assert!(validate_data(&enum_id, &(-1_i16).to_le_bytes(), &model, 0).is_ok());
        assert!(validate_data(&enum_id, &3_i16.to_le_bytes(), &model, 0).is_err());
    }

    #[test]
    fn resolves_omitted_baseline_integer_and_character_types() {
        let model = Model::default();
        assert_eq!(
            storage_for(
                "01010800-0000-0000-060e-2b3401040101",
                &model,
                &mut Vec::new(),
                0
            )
            .unwrap()
            .0,
            Storage::Data(Some(8))
        );
        assert!(validate_data("01100100-0000-0000-060e-2b3401040101", &[65, 0], &model, 0).is_ok());
        assert_eq!(
            integer_value(
                "01010800-0000-0000-060e-2b3401040101",
                &(-7_i64).to_le_bytes(),
                &model
            ),
            Some(-7)
        );
    }

    #[test]
    fn rejects_type_cycles() {
        let a = "00000000-0000-0000-0000-000000000001".to_owned();
        let b = "00000000-0000-0000-0000-000000000002".to_owned();
        let mut model = Model::default();
        for (id, next) in [(a.clone(), b.clone()), (b.clone(), a.clone())] {
            model.types.insert(
                id.clone(),
                TypeDef {
                    path: PathBuf::from(format!("/{id}")),
                    id,
                    name: "Rename".to_owned(),
                    kind: TypeKind::Rename { renamed_type: next },
                },
            );
        }
        assert!(storage_for(&a, &model, &mut Vec::new(), 0)
            .unwrap_err()
            .contains("cycle"));
    }

    #[test]
    fn rejects_truncated_dynamic_integer_value() {
        let type_id = "00000000-0000-0000-0000-000000000001".to_owned();
        let mut model = Model::default();
        model.types.insert(
            type_id.clone(),
            TypeDef {
                path: PathBuf::from("/int"),
                id: type_id.clone(),
                name: "ExtensionInt64".to_owned(),
                kind: TypeKind::Integer {
                    size: 8,
                    signed: true,
                },
            },
        );
        let object = StoredObject {
            path: PathBuf::from("/object"),
            class_id: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_owned(),
            properties: Vec::new(),
        };
        let value = StoredProperty {
            pid: 0x8001,
            format: SF_DATA,
            data: 42_i32.to_le_bytes().to_vec(),
        };
        let definition = PropertyDef {
            path: PathBuf::from("/definition"),
            id: "00000000-0000-0000-0000-000000000002".to_owned(),
            name: "TheAnswer".to_owned(),
            pid: value.pid,
            type_id,
            optional: true,
            unique: false,
        };
        let error = validate_value(
            &object,
            &value,
            &definition,
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &model,
        )
        .unwrap_err();
        assert!(error.contains("expected 8 data bytes"));
    }

    #[test]
    fn parses_aaf_auid_byte_order() {
        assert_eq!(
            auid_string(&id_bytes(0x00112233445566778899aabbccddeeff)).unwrap(),
            "00112233-4455-6677-8899-aabbccddeeff"
        );
    }
}
