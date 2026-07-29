//! Bounded AAF object-model and Edit Protocol validation.
//!
//! The stored-format reader lives in `aaf_qc`; this module works on its
//! already-bounded object/property/index representation.  It never resolves
//! external locators or reads essence streams.

use crate::container_qc::{check, AuditCheck};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use url::Url;

const MAX_FINDINGS: usize = 100;
const CLASS_PREFIX: &str = "0d010101-0101-";
const CLASS_SUFFIX: &str = "-060e-2b3402060101";
const EDIT_PROTOCOL: &str = "0d011201-0100-0000-060e-2b3404010105";
const DATADEF_PICTURE: &str = "01030202-0100-0000-060e-2b3404010101";
const DATADEF_SOUND: &str = "01030202-0200-0000-060e-2b3404010101";
const DATADEF_TIMECODE: &str = "01030201-0100-0000-060e-2b3404010101";
const DATADEF_LEGACY_PICTURE: &str = "6f3c8ce1-6cef-11d2-807d-006008143e6f";
const DATADEF_LEGACY_SOUND: &str = "78e1ebe1-6cef-11d2-807d-006008143e6f";
const USAGE_SUBCLIP: &str = "0d010102-0101-0500-060e-2b3404010101";
const USAGE_ADJUSTED_CLIP: &str = "0d010102-0101-0600-060e-2b3404010101";
const USAGE_TOP_LEVEL: &str = "0d010102-0101-0700-060e-2b3404010101";
const USAGE_LOWER_LEVEL: &str = "0d010102-0101-0800-060e-2b3404010101";
const USAGE_TEMPLATE: &str = "0d010102-0101-0900-060e-2b3404010101";

const SF_DATA: u16 = 0x82;
const SF_STRONG_REF: u16 = 0x22;
const SF_STRONG_VECTOR: u16 = 0x32;
const SF_STRONG_SET: u16 = 0x3a;
const SF_WEAK_REF: u16 = 0x02;
const SF_WEAK_VECTOR: u16 = 0x12;
const SF_WEAK_SET: u16 = 0x1a;

#[derive(Clone, Debug)]
pub(crate) struct StoredProperty {
    pub(crate) pid: u16,
    pub(crate) format: u16,
    pub(crate) data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub(crate) struct StoredObject {
    pub(crate) path: PathBuf,
    pub(crate) class_id: String,
    pub(crate) properties: Vec<StoredProperty>,
}

#[derive(Debug)]
pub(crate) struct ObjectAudit {
    pub(crate) checks: Vec<AuditCheck>,
    pub(crate) properties: Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Rational {
    numerator: i32,
    denominator: i32,
}

impl Rational {
    fn positive(self) -> bool {
        self.numerator > 0 && self.denominator > 0
    }

    fn equal(self, other: Self) -> bool {
        i64::from(self.numerator) * i64::from(other.denominator)
            == i64::from(other.numerator) * i64::from(self.denominator)
    }

    fn less_or_equal(self, other: Self) -> bool {
        i64::from(self.numerator) * i64::from(other.denominator)
            <= i64::from(other.numerator) * i64::from(self.denominator)
    }
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

    fn is_empty(&self) -> bool {
        self.total == 0
    }

    fn observed(&self) -> Option<Value> {
        (!self.is_empty()).then(|| {
            json!({
                "total": self.total,
                "reported": self.values
            })
        })
    }
}

pub(crate) fn audit(
    objects: &[StoredObject],
    streams: &HashMap<PathBuf, Vec<u8>>,
    stream_paths: &HashSet<PathBuf>,
) -> ObjectAudit {
    let by_path: HashMap<&Path, &StoredObject> = objects
        .iter()
        .map(|object| (object.path.as_path(), object))
        .collect();
    let mut checks = Vec::new();
    let mut warnings = Findings::default();

    let mut type_errors = Findings::default();
    let mut missing_required = Findings::default();
    let mut extension_classes = HashSet::new();
    for object in objects {
        let code = class_code(&object.class_id);
        if code.is_none()
            && !is_standard_meta_class(&object.class_id)
            && object.path != Path::new("/")
        {
            extension_classes.insert(object.class_id.clone());
        }
        validate_required_properties(object, code, &mut missing_required);
        validate_property_values(object, &mut type_errors);
    }
    checks.push(finding_check(
        "FORGE-AAF-OBJECT-REQUIRED-PROPERTIES",
        &missing_required,
        "all supported AAF classes contain their inherited required properties",
        "one or more supported AAF objects omit required properties",
    ));
    checks.push(finding_check(
        "FORGE-AAF-PROPERTY-TYPES",
        &type_errors,
        "known object properties have valid stored types and bounded values",
        "one or more known object properties have invalid stored values",
    ));

    let (references, reference_errors) = resolve_strong_references(objects, streams, &by_path);
    checks.push(finding_check(
        "FORGE-AAF-STRONG-REFERENCES",
        &reference_errors,
        "strong-reference names, indexes, and owned objects are consistent",
        "one or more strong-reference names or indexes are inconsistent",
    ));

    let meta_audit = crate::aaf_meta_qc::audit(objects, streams, stream_paths, &references);
    checks.extend(meta_audit.checks);

    let weak_reference_errors = validate_weak_references(objects, streams);
    checks.push(finding_check(
        "FORGE-AAF-WEAK-REFERENCES",
        &weak_reference_errors,
        "weak-reference payloads and indexes resolve to unique in-file objects",
        "one or more weak references are malformed or unresolved",
    ));

    let mut owned = HashMap::<&Path, usize>::new();
    for targets in references.values() {
        for target in targets {
            *owned.entry(target.as_path()).or_default() += 1;
        }
    }
    let mut ownership_errors = Findings::default();
    for object in objects
        .iter()
        .filter(|object| object.path != Path::new("/"))
    {
        match owned.get(object.path.as_path()).copied().unwrap_or(0) {
            1 => {}
            0 => ownership_errors.push(format!(
                "{} is not owned by a strong reference",
                object.path.display()
            )),
            count => ownership_errors.push(format!(
                "{} is owned by {count} strong references",
                object.path.display()
            )),
        }
    }
    checks.push(finding_check(
        "FORGE-AAF-OBJECT-OWNERSHIP",
        &ownership_errors,
        "every stored object has exactly one strong-reference owner",
        "one or more stored objects have invalid ownership",
    ));

    let mut ids = Findings::default();
    validate_unique_identifiers(objects, &mut ids);
    checks.push(finding_check(
        "FORGE-AAF-UNIQUE-IDENTIFIERS",
        &ids,
        "Mob, EssenceData, and DefinitionObject unique identifiers are present and unique",
        "one or more object identifiers are duplicated or malformed",
    ));

    let mut timeline = Findings::default();
    let mut source = Findings::default();
    let mut effects = Findings::default();
    let summary = validate_edit_graph(
        objects,
        &by_path,
        &references,
        &mut timeline,
        &mut source,
        &mut effects,
        &mut warnings,
    );
    checks.push(finding_check(
        "FORGE-AAF-EDIT-TIMELINE",
        &timeline,
        "Mob slots, edit rates, component lengths, sequences, and transitions are coherent",
        "one or more edit timeline constraints fail",
    ));
    checks.push(finding_check(
        "FORGE-AAF-SOURCE-REFERENCES",
        &source,
        "Mob derivation references and source slot mappings are coherent",
        "one or more Mob derivation references are invalid",
    ));
    checks.push(finding_check(
        "FORGE-AAF-EFFECTS",
        &effects,
        "operation inputs, parameters, nested scopes, and varying values are coherent",
        "one or more effect-model constraints fail",
    ));
    let effect_profile_audit = crate::aaf_effect_qc::audit(objects, streams, &references);
    checks.push(effect_profile_audit.check);

    let mut protocol = Findings::default();
    validate_edit_protocol(objects, &references, &summary, &mut protocol, &mut warnings);
    checks.push(finding_check(
        "FORGE-AAF-EDIT-PROTOCOL",
        &protocol,
        "supported AAF Edit Protocol material, track, and locator constraints pass",
        "one or more supported AAF Edit Protocol constraints fail",
    ));

    if !extension_classes.is_empty() {
        warnings.push(format!(
            "{} extension class identifier(s) were preserved but not semantically interpreted",
            extension_classes.len()
        ));
    }
    ObjectAudit {
        checks,
        properties: json!({
            "objects": objects.len().saturating_sub(1),
            "strong_references": references.values().map(Vec::len).sum::<usize>(),
            "mobs": summary.mob_count,
            "composition_mobs": summary.composition_count,
            "master_mobs": summary.master_count,
            "source_mobs": summary.source_count,
            "slots": summary.slot_count,
            "components": summary.component_count,
            "extension_classes": extension_classes.len(),
            "meta_dictionary": meta_audit.properties,
            "effect_profiles": effect_profile_audit.properties,
            "warning_count": warnings.total,
            "warnings": warnings.values
        }),
    }
}

fn finding_check(
    rule_id: &'static str,
    findings: &Findings,
    passed_message: &'static str,
    failed_message: &'static str,
) -> AuditCheck {
    check(
        rule_id,
        findings.is_empty(),
        if findings.is_empty() {
            passed_message
        } else {
            failed_message
        },
        findings.observed(),
    )
}

fn class_code(class_id: &str) -> Option<u16> {
    let value = class_id
        .strip_prefix(CLASS_PREFIX)?
        .strip_suffix(CLASS_SUFFIX)?;
    (value.len() == 4)
        .then(|| u16::from_str_radix(&value[..2], 16).ok())
        .flatten()
}

fn is_standard_meta_class(class_id: &str) -> bool {
    class_id.ends_with(CLASS_SUFFIX)
        && (class_id.starts_with("0d010101-02")
            || class_id.starts_with("0d010400-")
            || class_id.starts_with("0d010401-")
            || class_id.starts_with("0e040101-"))
}

fn property(object: &StoredObject, pid: u16) -> Option<&StoredProperty> {
    object
        .properties
        .iter()
        .find(|property| property.pid == pid)
}

fn validate_required_properties(object: &StoredObject, code: Option<u16>, findings: &mut Findings) {
    let Some(code) = code else {
        return;
    };
    let mut required = Vec::new();
    if matches!(code, 0x0002..=0x0017) {
        required.push(0x0201);
    }
    if matches!(code, 0x001a..=0x0021 | 0x0024..=0x002e) {
        // DefinitionObject and EssenceDescriptor themselves are abstract, but
        // their concrete subclasses inherit these requirements where present.
        if matches!(code, 0x001a..=0x0021) {
            required.extend([0x1b01, 0x1b02]);
        }
    }
    if matches!(code, 0x0025..=0x002c) {
        required.extend([0x3001, 0x3002]);
    }
    if matches!(code, 0x0034..=0x0037) {
        required.extend([0x4401, 0x4403, 0x4404, 0x4405]);
    }
    if matches!(code, 0x0038..=0x003b) {
        required.extend([0x4801, 0x4803]);
    }
    required.extend(match code {
        0x0004 => vec![0x0401, 0x0402, 0x0403],
        0x0005 => vec![0x0501],
        0x0006 => vec![0x0601],
        0x0007 => vec![0x0801],
        0x000a => vec![0x0b01],
        0x000b => vec![0x0c01],
        0x000c => vec![0x0d01, 0x0d02, 0x0d03, 0x0d04],
        0x000d => vec![0x0e01, 0x0e02],
        0x000e => vec![0x0f01],
        0x000f => vec![0x1001],
        0x0010..=0x0013 => vec![0x1102],
        0x0014 => vec![0x1501, 0x1502, 0x1503],
        0x0015 => vec![0x1601, 0x1602, 0x1603],
        0x0016 => vec![0x1701],
        0x0017 => vec![0x1801, 0x1802],
        0x0018 => vec![0x1901],
        0x0019 => vec![0x1a02, 0x1a03],
        0x001c => vec![0x1e01, 0x1e07],
        0x001d => vec![0x1f01],
        0x001f => vec![0x2301, 0x2302],
        0x0022 => Vec::new(),
        0x0023 => vec![0x2701, 0x2702],
        0x0026 => vec![0x3101],
        0x0027 => vec![0x3202, 0x3203, 0x320c, 0x320d, 0x320e],
        0x0028 => vec![0x3301, 0x3302],
        0x0029 => vec![0x3401],
        0x002b => vec![0x3701, 0x3702, 0x3706],
        0x002c => vec![0x3801],
        0x002f => vec![0x3b01, 0x3b02, 0x3b03, 0x3b04, 0x3b05, 0x3b06],
        0x0030 => vec![0x3c01, 0x3c02, 0x3c04, 0x3c05, 0x3c06, 0x3c09],
        0x0032 => vec![0x4001],
        0x0033 => vec![0x4101],
        0x0037 => vec![0x4701],
        0x0039 => vec![0x4901],
        0x003b => vec![0x4b01, 0x4b02],
        0x003c => vec![0x4c01],
        0x003d => vec![0x4d01],
        0x003e => vec![0x4e01, 0x4e02],
        0x003f => vec![0x5001, 0x5003],
        0x0040 => vec![0x5101],
        _ => Vec::new(),
    });
    required.sort_unstable();
    required.dedup();
    for pid in required {
        if property(object, pid).is_none() {
            findings.push(format!(
                "{} ({}) is missing PID 0x{pid:04x}",
                object.path.display(),
                object.class_id
            ));
        }
    }
}

fn validate_property_values(object: &StoredObject, findings: &mut Findings) {
    for property in &object.properties {
        let length = property.data.len();
        let valid = match property.pid {
            // aafUInt32 / aafInt32 / SlotID / PhysicalTrackNumber
            0x1102
            | 0x2401
            | 0x3006
            | 0x3202..=0x320b
            | 0x3301
            | 0x3302
            | 0x3304..=0x3306
            | 0x3308
            | 0x3309
            | 0x3703..=0x3705
            | 0x3902
            | 0x3a04
            | 0x4801
            | 0x4804 => length == 4,
            0x1502 | 0x3307 => length == 2,
            0x3303 => length == 1,
            // Position / Length
            0x0202
            | 0x0401
            | 0x0601
            | 0x1201
            | 0x1202
            | 0x1204
            | 0x1501
            | 0x1802
            | 0x3002
            | 0x4501
            | 0x4902
            | 0x4b02..=0x4b05 => length == 8,
            // Rational
            0x1601 | 0x1a03 | 0x3001 | 0x320e | 0x3904 | 0x4503 | 0x4901 | 0x4b01 => {
                rational(property).is_some()
            }
            // AUID / extendible enum / class ID
            0x1b01
            | 0x2208..=0x2211
            | 0x2301
            | 0x2f01
            | 0x3004
            | 0x3005
            | 0x3201
            | 0x3b08..=0x3b0b
            | 0x3c05
            | 0x3c09
            | 0x4408
            | 0x4c01 => length == 16 || property.format != SF_DATA,
            // MobID
            0x1101 | 0x2701 | 0x4401 | 0x4504 => length == 32,
            // UTF-16 strings
            0x1b02
            | 0x1b03
            | 0x2204..=0x2207
            | 0x3c01
            | 0x3c02
            | 0x3c04
            | 0x3c08
            | 0x4001
            | 0x4101
            | 0x4402
            | 0x4802
            | 0x5001 => utf16_string(&property.data).is_some(),
            _ => true,
        };
        if !valid {
            findings.push(format!(
                "{} PID 0x{:04x} has invalid {}-byte data for format 0x{:02x}",
                object.path.display(),
                property.pid,
                length,
                property.format
            ));
        }
        if matches!(property.format, SF_WEAK_REF) && weak_key(property).is_none() {
            findings.push(format!(
                "{} PID 0x{:04x} has a malformed weak reference",
                object.path.display(),
                property.pid
            ));
        }
    }
}

fn resolve_strong_references(
    objects: &[StoredObject],
    streams: &HashMap<PathBuf, Vec<u8>>,
    by_path: &HashMap<&Path, &StoredObject>,
) -> (HashMap<(PathBuf, u16), Vec<PathBuf>>, Findings) {
    let mut result = HashMap::new();
    let mut findings = Findings::default();
    for object in objects {
        for property in &object.properties {
            if !matches!(
                property.format,
                SF_STRONG_REF | SF_STRONG_VECTOR | SF_STRONG_SET
            ) {
                continue;
            }
            let Some(name) = strong_reference_name(&property.data) else {
                findings.push(format!(
                    "{} PID 0x{:04x} has an invalid UTF-16 reference name",
                    object.path.display(),
                    property.pid
                ));
                continue;
            };
            if !safe_child_name(&name) {
                findings.push(format!(
                    "{} PID 0x{:04x} has unsafe reference name {name:?}",
                    object.path.display(),
                    property.pid
                ));
                continue;
            }
            let keys = match property.format {
                SF_STRONG_REF => Ok(vec![None]),
                SF_STRONG_VECTOR => parse_vector_index(object, &name, streams)
                    .map(|keys| keys.into_iter().map(Some).collect()),
                SF_STRONG_SET => parse_set_index(object, &name, streams)
                    .map(|keys| keys.into_iter().map(Some).collect()),
                _ => unreachable!(),
            };
            let keys = match keys {
                Ok(keys) => keys,
                Err(error) => {
                    findings.push(format!(
                        "{} PID 0x{:04x}: {error}",
                        object.path.display(),
                        property.pid
                    ));
                    continue;
                }
            };
            let mut targets = Vec::with_capacity(keys.len());
            let mut unique = HashSet::new();
            for key in keys {
                let child = match key {
                    Some(key) => format!("{name}{{{key:x}}}"),
                    None => name.clone(),
                };
                let target = object.path.join(child);
                if !unique.insert(target.clone()) {
                    findings.push(format!(
                        "{} PID 0x{:04x} repeats {}",
                        object.path.display(),
                        property.pid,
                        target.display()
                    ));
                } else if !by_path.contains_key(target.as_path()) {
                    findings.push(format!(
                        "{} PID 0x{:04x} references missing object {}",
                        object.path.display(),
                        property.pid,
                        target.display()
                    ));
                }
                targets.push(target);
            }
            result.insert((object.path.clone(), property.pid), targets);
        }
    }
    (result, findings)
}

fn strong_reference_name(data: &[u8]) -> Option<String> {
    utf16_string(data).filter(|name| !name.is_empty())
}

fn safe_child_name(value: &str) -> bool {
    value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !value.chars().any(char::is_control)
}

fn parse_vector_index(
    object: &StoredObject,
    name: &str,
    streams: &HashMap<PathBuf, Vec<u8>>,
) -> Result<Vec<u32>, String> {
    let path = object.path.join(format!("{name} index"));
    let bytes = streams
        .get(&path)
        .ok_or_else(|| format!("missing vector index {}", path.display()))?;
    if bytes.len() < 12 {
        return Err("truncated strong-reference vector index".to_owned());
    }
    let count = read_u32(bytes, 0)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| "invalid vector count".to_owned())?;
    let expected = count
        .checked_mul(4)
        .and_then(|value| value.checked_add(12))
        .ok_or_else(|| "vector index length overflow".to_owned())?;
    if bytes.len() != expected {
        return Err(format!(
            "vector index declares {count} entries but has {} bytes",
            bytes.len()
        ));
    }
    let mut keys = Vec::with_capacity(count);
    for offset in (12..expected).step_by(4) {
        keys.push(read_u32(bytes, offset).unwrap());
    }
    if keys.iter().copied().collect::<HashSet<_>>().len() != keys.len() {
        return Err("vector index repeats a local key".to_owned());
    }
    Ok(keys)
}

fn parse_set_index(
    object: &StoredObject,
    name: &str,
    streams: &HashMap<PathBuf, Vec<u8>>,
) -> Result<Vec<u32>, String> {
    let path = object.path.join(format!("{name} index"));
    let bytes = streams
        .get(&path)
        .ok_or_else(|| format!("missing set index {}", path.display()))?;
    if bytes.len() < 15 {
        return Err("truncated strong-reference set index".to_owned());
    }
    let count = read_u32(bytes, 0)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| "invalid set count".to_owned())?;
    let key_size = usize::from(bytes[14]);
    if !matches!(key_size, 16 | 32) {
        return Err(format!("unsupported set unique-key size {key_size}"));
    }
    let record_size = 8 + key_size;
    let expected = count
        .checked_mul(record_size)
        .and_then(|value| value.checked_add(15))
        .ok_or_else(|| "set index length overflow".to_owned())?;
    if bytes.len() != expected {
        return Err(format!(
            "set index declares {count} entries but has {} bytes",
            bytes.len()
        ));
    }
    let mut keys = Vec::with_capacity(count);
    let mut unique_values = HashSet::new();
    for index in 0..count {
        let offset = 15 + index * record_size;
        let key = read_u32(bytes, offset).unwrap();
        let ref_count = read_u32(bytes, offset + 4).unwrap();
        if ref_count != 1 {
            return Err(format!("set entry {index} has reference count {ref_count}"));
        }
        if !unique_values.insert(&bytes[offset + 8..offset + record_size]) {
            return Err("set index repeats a unique key".to_owned());
        }
        keys.push(key);
    }
    if keys.iter().copied().collect::<HashSet<_>>().len() != keys.len() {
        return Err("set index repeats a local key".to_owned());
    }
    Ok(keys)
}

fn validate_weak_references(
    objects: &[StoredObject],
    streams: &HashMap<PathBuf, Vec<u8>>,
) -> Findings {
    let mut findings = Findings::default();
    let unique_keys: HashSet<(u16, Vec<u8>)> = objects
        .iter()
        .flat_map(|object| object.properties.iter())
        .filter(|property| {
            matches!(property.pid, 0x0005 | 0x1b01 | 0x2701 | 0x4401)
                && matches!(property.data.len(), 16 | 32)
        })
        .map(|property| (property.pid, property.data.clone()))
        .collect();
    for object in objects {
        for property in &object.properties {
            match property.format {
                SF_WEAK_REF => {
                    let Some((key_pid, key)) = parse_weak_reference(&property.data) else {
                        findings.push(format!(
                            "{} PID 0x{:04x} has a malformed weak reference",
                            object.path.display(),
                            property.pid
                        ));
                        continue;
                    };
                    if !unique_keys.contains(&(key_pid, key.to_vec()))
                        && !is_baseline_meta_key(key_pid, key)
                    {
                        findings.push(format!(
                            "{} PID 0x{:04x} has an unresolved weak reference to key PID 0x{key_pid:04x}",
                            object.path.display(),
                            property.pid
                        ));
                    }
                }
                SF_WEAK_VECTOR | SF_WEAK_SET => {
                    let Some(name) = strong_reference_name(&property.data) else {
                        findings.push(format!(
                            "{} PID 0x{:04x} has an invalid weak-index name",
                            object.path.display(),
                            property.pid
                        ));
                        continue;
                    };
                    let path = object.path.join(format!("{name} index"));
                    let Some(bytes) = streams.get(&path) else {
                        findings.push(format!(
                            "{} PID 0x{:04x} is missing weak index {}",
                            object.path.display(),
                            property.pid,
                            path.display()
                        ));
                        continue;
                    };
                    let entries = match parse_weak_index(bytes, property.format == SF_WEAK_SET) {
                        Ok(entries) => entries,
                        Err(error) => {
                            findings.push(format!(
                                "{} PID 0x{:04x}: {error}",
                                object.path.display(),
                                property.pid
                            ));
                            continue;
                        }
                    };
                    for (key_pid, key) in entries {
                        if !unique_keys.contains(&(key_pid, key.clone()))
                            && !is_baseline_meta_key(key_pid, &key)
                        {
                            findings.push(format!(
                                "{} PID 0x{:04x} weak index has an unresolved key for PID 0x{key_pid:04x}",
                                object.path.display(),
                                property.pid
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    findings
}

fn parse_weak_reference(bytes: &[u8]) -> Option<(u16, &[u8])> {
    if bytes.len() < 5 {
        return None;
    }
    let key_pid = u16::from_le_bytes(bytes[2..4].try_into().ok()?);
    let key_size = usize::from(bytes[4]);
    if !matches!(key_size, 16 | 32) || bytes.len() != 5 + key_size {
        return None;
    }
    Some((key_pid, &bytes[5..]))
}

fn is_baseline_meta_key(key_pid: u16, key: &[u8]) -> bool {
    key_pid == 0x0005 && auid_string(key).is_ok_and(|value| value.ends_with("-060e-2b3401040101"))
}

fn parse_weak_index(bytes: &[u8], require_unique: bool) -> Result<Vec<(u16, Vec<u8>)>, String> {
    if bytes.len() < 9 {
        return Err("truncated weak-reference vector/set index".to_owned());
    }
    let count = read_u32(bytes, 0)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| "invalid weak-reference count".to_owned())?;
    let key_pid = u16::from_le_bytes(bytes[6..8].try_into().unwrap());
    let key_size = usize::from(bytes[8]);
    if !matches!(key_size, 16 | 32) {
        return Err(format!("unsupported weak-reference key size {key_size}"));
    }
    let expected = count
        .checked_mul(key_size)
        .and_then(|value| value.checked_add(9))
        .ok_or_else(|| "weak-reference index length overflow".to_owned())?;
    if bytes.len() != expected {
        return Err(format!(
            "weak-reference index declares {count} entries but has {} bytes",
            bytes.len()
        ));
    }
    let mut unique = HashSet::new();
    let mut result = Vec::with_capacity(count);
    for key in bytes[9..].chunks_exact(key_size) {
        if require_unique && !unique.insert(key) {
            return Err("weak-reference index repeats a key".to_owned());
        }
        result.push((key_pid, key.to_vec()));
    }
    Ok(result)
}

fn validate_unique_identifiers(objects: &[StoredObject], findings: &mut Findings) {
    for (pid, size, label, codes) in [
        (0x4401, 32, "MobID", &[0x0034, 0x0035, 0x0036, 0x0037][..]),
        (0x2701, 32, "EssenceData MobID", &[0x0023][..]),
        (
            0x1b01,
            16,
            "DefinitionObject Identification",
            &[
                0x001a, 0x001b, 0x001c, 0x001d, 0x001e, 0x001f, 0x0020, 0x0021,
            ][..],
        ),
    ] {
        let mut seen = HashMap::<Vec<u8>, &Path>::new();
        for object in objects
            .iter()
            .filter(|object| class_code(&object.class_id).is_some_and(|code| codes.contains(&code)))
        {
            let Some(value) = property(object, pid) else {
                continue;
            };
            if value.data.len() != size || value.data.iter().all(|byte| *byte == 0) {
                findings.push(format!("{} has malformed {label}", object.path.display()));
            } else if let Some(previous) = seen.insert(value.data.clone(), &object.path) {
                findings.push(format!(
                    "{} duplicates {label} from {}",
                    object.path.display(),
                    previous.display()
                ));
            }
        }
    }
}

#[derive(Default)]
struct EditSummary {
    mob_count: usize,
    composition_count: usize,
    master_count: usize,
    source_count: usize,
    slot_count: usize,
    component_count: usize,
    mob_ids: HashMap<Vec<u8>, PathBuf>,
    mob_slots: HashMap<PathBuf, HashMap<u32, PathBuf>>,
    source_edges: Vec<(PathBuf, PathBuf)>,
    audio_rates: Vec<(PathBuf, Rational)>,
}

#[allow(clippy::too_many_arguments)]
fn validate_edit_graph(
    objects: &[StoredObject],
    by_path: &HashMap<&Path, &StoredObject>,
    references: &HashMap<(PathBuf, u16), Vec<PathBuf>>,
    timeline: &mut Findings,
    source: &mut Findings,
    effects: &mut Findings,
    warnings: &mut Findings,
) -> EditSummary {
    let mut summary = EditSummary::default();
    for object in objects {
        match class_code(&object.class_id) {
            Some(0x0034..=0x0037) => {
                summary.mob_count += 1;
                if let Some(id) = property(object, 0x4401) {
                    summary.mob_ids.insert(id.data.clone(), object.path.clone());
                }
                match class_code(&object.class_id) {
                    Some(0x0035) => summary.composition_count += 1,
                    Some(0x0036) => summary.master_count += 1,
                    Some(0x0037) => summary.source_count += 1,
                    _ => {}
                }
            }
            Some(0x0038..=0x003b) => summary.slot_count += 1,
            Some(0x0002..=0x0017) => summary.component_count += 1,
            _ => {}
        }
    }

    for mob in objects
        .iter()
        .filter(|object| matches!(class_code(&object.class_id), Some(0x0034..=0x0037)))
    {
        let slots = references
            .get(&(mob.path.clone(), 0x4403))
            .cloned()
            .unwrap_or_default();
        let mut ids = HashMap::new();
        for slot_path in &slots {
            let Some(slot) = by_path.get(slot_path.as_path()).copied() else {
                continue;
            };
            if let Some(id) = property(slot, 0x4801).and_then(|value| read_u32(&value.data, 0)) {
                if id == 0 {
                    timeline.push(format!("{} has zero SlotID", slot.path.display()));
                }
                if ids.insert(id, slot.path.clone()).is_some() {
                    timeline.push(format!("{} repeats SlotID {id}", mob.path.display()));
                }
            }
            validate_slot(slot, by_path, references, timeline, effects, &mut summary);
        }
        summary.mob_slots.insert(mob.path.clone(), ids);
    }

    validate_source_clips(objects, by_path, references, &mut summary, source, warnings);
    validate_source_cycles(&summary.source_edges, source);
    validate_effect_definitions(objects, by_path, references, effects);
    validate_scope_references(objects, by_path, references, effects);
    summary
}

#[allow(clippy::too_many_arguments)]
fn validate_slot(
    slot: &StoredObject,
    by_path: &HashMap<&Path, &StoredObject>,
    references: &HashMap<(PathBuf, u16), Vec<PathBuf>>,
    timeline: &mut Findings,
    effects: &mut Findings,
    summary: &mut EditSummary,
) {
    let segment = references
        .get(&(slot.path.clone(), 0x4803))
        .and_then(|targets| targets.first())
        .and_then(|path| by_path.get(path.as_path()).copied());
    let Some(segment) = segment else {
        return;
    };
    let rate = property(slot, 0x4b01).and_then(rational);
    if class_code(&slot.class_id) == Some(0x003b) {
        if !rate.is_some_and(Rational::positive) {
            timeline.push(format!("{} has an invalid EditRate", slot.path.display()));
        }
        for pid in [0x4b03, 0x4b04, 0x4b05] {
            if let Some(value) = property(slot, pid).and_then(signed_i64) {
                if value < 0 {
                    timeline.push(format!(
                        "{} PID 0x{pid:04x} has negative position {value}",
                        slot.path.display()
                    ));
                }
            }
        }
        if let (Some(mark_in), Some(mark_out)) = (
            property(slot, 0x4b03).and_then(signed_i64),
            property(slot, 0x4b04).and_then(signed_i64),
        ) {
            if mark_in > mark_out {
                timeline.push(format!(
                    "{} has MarkIn {mark_in} after MarkOut {mark_out}",
                    slot.path.display()
                ));
            }
        }
    }
    let data_definition = component_data_definition(segment);
    if data_definition.as_deref().is_some_and(is_sound_definition) {
        if let Some(rate) = rate {
            summary.audio_rates.push((slot.path.clone(), rate));
        }
    }
    validate_component(
        segment,
        data_definition.as_deref(),
        by_path,
        references,
        timeline,
        effects,
        &mut HashSet::new(),
    );
}

#[allow(clippy::too_many_arguments)]
fn validate_component(
    component: &StoredObject,
    expected_data_definition: Option<&str>,
    by_path: &HashMap<&Path, &StoredObject>,
    references: &HashMap<(PathBuf, u16), Vec<PathBuf>>,
    timeline: &mut Findings,
    effects: &mut Findings,
    visited: &mut HashSet<PathBuf>,
) -> Option<i64> {
    if !visited.insert(component.path.clone()) {
        effects.push(format!(
            "{} forms a recursive component ownership graph",
            component.path.display()
        ));
        return None;
    }
    let definition = component_data_definition(component);
    if let (Some(expected), Some(actual)) = (expected_data_definition, definition.as_deref()) {
        if expected != actual {
            timeline.push(format!(
                "{} DataDefinition {actual} differs from parent {expected}",
                component.path.display()
            ));
        }
    }
    let length = property(component, 0x0202).and_then(signed_i64);
    if length.is_some_and(|value| value < 0) {
        timeline.push(format!(
            "{} has negative component Length",
            component.path.display()
        ));
    }
    match class_code(&component.class_id) {
        Some(0x000f) => {
            let children = references
                .get(&(component.path.clone(), 0x1001))
                .cloned()
                .unwrap_or_default();
            if children.is_empty() {
                timeline.push(format!(
                    "{} has an empty Sequence",
                    component.path.display()
                ));
            }
            let mut calculated = 0_i64;
            let mut complete = true;
            for (index, child_path) in children.iter().enumerate() {
                let Some(child) = by_path.get(child_path.as_path()).copied() else {
                    complete = false;
                    continue;
                };
                let child_length = validate_component(
                    child,
                    definition.as_deref(),
                    by_path,
                    references,
                    timeline,
                    effects,
                    visited,
                );
                if class_code(&child.class_id) == Some(0x0017) {
                    if index == 0 || index + 1 == children.len() {
                        timeline.push(format!(
                            "{} has a Transition at a Sequence boundary",
                            child.path.display()
                        ));
                    }
                    if let (Some(value), Some(cut)) =
                        (child_length, property(child, 0x1802).and_then(signed_i64))
                    {
                        if cut < 0 || cut > value {
                            timeline.push(format!(
                                "{} CutPoint {cut} is outside transition length {value}",
                                child.path.display()
                            ));
                        }
                        for adjacent_path in
                            [children.get(index.wrapping_sub(1)), children.get(index + 1)]
                                .into_iter()
                                .flatten()
                        {
                            if by_path
                                .get(adjacent_path.as_path())
                                .and_then(|adjacent| property(adjacent, 0x0202))
                                .and_then(signed_i64)
                                .is_some_and(|adjacent_length| adjacent_length < value)
                            {
                                timeline.push(format!(
                                    "{} transition length {value} exceeds adjacent component {}",
                                    child.path.display(),
                                    adjacent_path.display()
                                ));
                            }
                        }
                        calculated = calculated.saturating_sub(value);
                    } else {
                        complete = false;
                    }
                } else if let Some(value) = child_length {
                    calculated = calculated.saturating_add(value);
                } else {
                    complete = false;
                }
            }
            if complete && length.is_some_and(|declared| declared != calculated) {
                timeline.push(format!(
                    "{} declares Length {} but components calculate to {calculated}",
                    component.path.display(),
                    length.unwrap()
                ));
            }
        }
        Some(0x000a) => validate_operation_group(
            component,
            definition.as_deref(),
            by_path,
            references,
            timeline,
            effects,
            visited,
        ),
        Some(0x000b) => {
            let slots = references
                .get(&(component.path.clone(), 0x0c01))
                .cloned()
                .unwrap_or_default();
            if slots.is_empty() {
                effects.push(format!(
                    "{} has an empty NestedScope",
                    component.path.display()
                ));
            }
            let mut lengths = Vec::new();
            for path in slots {
                if let Some(child) = by_path.get(path.as_path()).copied() {
                    if let Some(value) = validate_component(
                        child,
                        definition.as_deref(),
                        by_path,
                        references,
                        timeline,
                        effects,
                        visited,
                    ) {
                        lengths.push(value);
                    }
                }
            }
            if lengths.windows(2).any(|pair| pair[0] != pair[1]) {
                effects.push(format!(
                    "{} NestedScope slots have different lengths",
                    component.path.display()
                ));
            }
        }
        Some(0x000e) => {
            for pid in [0x0f01, 0x0f02] {
                for path in references
                    .get(&(component.path.clone(), pid))
                    .into_iter()
                    .flatten()
                {
                    if let Some(child) = by_path.get(path.as_path()).copied() {
                        validate_component(
                            child,
                            definition.as_deref(),
                            by_path,
                            references,
                            timeline,
                            effects,
                            visited,
                        );
                    }
                }
            }
        }
        Some(0x0017) => {
            if let Some(operation) = references
                .get(&(component.path.clone(), 0x1801))
                .and_then(|targets| targets.first())
                .and_then(|path| by_path.get(path.as_path()).copied())
            {
                if references
                    .get(&(operation.path.clone(), 0x0b02))
                    .is_some_and(|values| !values.is_empty())
                {
                    effects.push(format!(
                        "{} Transition OperationGroup must use implicit inputs",
                        operation.path.display()
                    ));
                }
                validate_component(
                    operation,
                    definition.as_deref(),
                    by_path,
                    references,
                    timeline,
                    effects,
                    visited,
                );
            }
        }
        _ => {}
    }
    visited.remove(&component.path);
    length
}

#[allow(clippy::too_many_arguments)]
fn validate_operation_group(
    operation: &StoredObject,
    definition: Option<&str>,
    by_path: &HashMap<&Path, &StoredObject>,
    references: &HashMap<(PathBuf, u16), Vec<PathBuf>>,
    timeline: &mut Findings,
    effects: &mut Findings,
    visited: &mut HashSet<PathBuf>,
) {
    let inputs = references
        .get(&(operation.path.clone(), 0x0b02))
        .cloned()
        .unwrap_or_default();
    for input in inputs {
        if let Some(child) = by_path.get(input.as_path()).copied() {
            validate_component(
                child, definition, by_path, references, timeline, effects, visited,
            );
        }
    }
    let parameters = references
        .get(&(operation.path.clone(), 0x0b03))
        .cloned()
        .unwrap_or_default();
    let mut definitions = HashSet::new();
    for path in parameters {
        let Some(parameter) = by_path.get(path.as_path()).copied() else {
            continue;
        };
        if let Some(value) = property(parameter, 0x4c01) {
            if !definitions.insert(value.data.clone()) {
                effects.push(format!(
                    "{} repeats a parameter definition",
                    operation.path.display()
                ));
            }
        }
        if class_code(&parameter.class_id) == Some(0x003e) {
            validate_varying_value(parameter, by_path, references, effects);
        }
    }
}

fn validate_varying_value(
    varying: &StoredObject,
    by_path: &HashMap<&Path, &StoredObject>,
    references: &HashMap<(PathBuf, u16), Vec<PathBuf>>,
    effects: &mut Findings,
) {
    let points = references
        .get(&(varying.path.clone(), 0x4e02))
        .cloned()
        .unwrap_or_default();
    if points.is_empty() {
        effects.push(format!(
            "{} has no VaryingValue control points",
            varying.path.display()
        ));
        return;
    }
    let mut previous = None;
    for path in points {
        let time = by_path
            .get(path.as_path())
            .and_then(|point| property(point, 0x1a03))
            .and_then(rational);
        let Some(time) = time else {
            continue;
        };
        if previous.is_some_and(|last: Rational| !last.less_or_equal(time) || last.equal(time)) {
            effects.push(format!(
                "{} control-point times are not strictly increasing",
                varying.path.display()
            ));
        }
        previous = Some(time);
    }
}

fn validate_source_clips(
    objects: &[StoredObject],
    by_path: &HashMap<&Path, &StoredObject>,
    references: &HashMap<(PathBuf, u16), Vec<PathBuf>>,
    summary: &mut EditSummary,
    findings: &mut Findings,
    warnings: &mut Findings,
) {
    for clip in objects
        .iter()
        .filter(|object| class_code(&object.class_id) == Some(0x0011))
    {
        let Some(source_id) = property(clip, 0x1101) else {
            continue;
        };
        if source_id.data.iter().all(|byte| *byte == 0) {
            continue;
        }
        let owner_mob = ancestor_mob(clip, by_path);
        let target_mob = summary.mob_ids.get(&source_id.data).cloned();
        let Some(target_mob) = target_mob else {
            warnings.push(format!(
                "{} references an unresolved Mob; external SourceMob references are permitted",
                clip.path.display()
            ));
            continue;
        };
        if let Some(owner) = owner_mob {
            summary.source_edges.push((owner, target_mob.clone()));
        }
        let slot_id = property(clip, 0x1102).and_then(|value| read_u32(&value.data, 0));
        let target_slot = slot_id.and_then(|id| {
            summary
                .mob_slots
                .get(&target_mob)
                .and_then(|slots| slots.get(&id))
        });
        if target_slot.is_none() {
            findings.push(format!(
                "{} references missing SlotID {} in {}",
                clip.path.display(),
                slot_id
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "<invalid>".to_owned()),
                target_mob.display()
            ));
            continue;
        }
        if let (Some(start), Some(length), Some(target_length)) = (
            property(clip, 0x1201).and_then(signed_i64),
            property(clip, 0x0202).and_then(signed_i64),
            target_slot
                .and_then(|path| references.get(&(path.clone(), 0x4803)))
                .and_then(|targets| targets.first())
                .and_then(|path| by_path.get(path.as_path()))
                .and_then(|segment| property(segment, 0x0202))
                .and_then(signed_i64),
        ) {
            if start < 0 || length < 0 || start.saturating_add(length) > target_length {
                warnings.push(format!(
                    "{} source extent {start}+{length} exceeds target extent {target_length}; transition fallback may apply",
                    clip.path.display()
                ));
            }
        }
    }
}

fn ancestor_mob(object: &StoredObject, by_path: &HashMap<&Path, &StoredObject>) -> Option<PathBuf> {
    let mut path = object.path.parent();
    while let Some(candidate) = path {
        if by_path
            .get(candidate)
            .is_some_and(|value| matches!(class_code(&value.class_id), Some(0x0034..=0x0037)))
        {
            return Some(candidate.to_path_buf());
        }
        path = candidate.parent();
    }
    None
}

fn validate_source_cycles(edges: &[(PathBuf, PathBuf)], findings: &mut Findings) {
    let adjacency: HashMap<&Path, Vec<&Path>> = {
        let mut result = HashMap::<&Path, Vec<&Path>>::new();
        for (from, to) in edges {
            result.entry(from).or_default().push(to);
        }
        result
    };
    for start in adjacency.keys().copied() {
        let mut stack = vec![(start, HashSet::from([start]))];
        while let Some((node, visited)) = stack.pop() {
            for next in adjacency.get(node).into_iter().flatten().copied() {
                if next == start {
                    findings.push(format!(
                        "Mob derivation cycle returns to {}",
                        start.display()
                    ));
                    break;
                }
                if !visited.contains(next) {
                    let mut branch = visited.clone();
                    branch.insert(next);
                    stack.push((next, branch));
                }
            }
        }
    }
}

fn validate_effect_definitions(
    objects: &[StoredObject],
    by_path: &HashMap<&Path, &StoredObject>,
    references: &HashMap<(PathBuf, u16), Vec<PathBuf>>,
    findings: &mut Findings,
) {
    let definitions: HashMap<Vec<u8>, &StoredObject> = objects
        .iter()
        .filter(|object| class_code(&object.class_id) == Some(0x001c))
        .filter_map(|object| property(object, 0x1b01).map(|id| (id.data.clone(), object)))
        .collect();
    for operation in objects
        .iter()
        .filter(|object| class_code(&object.class_id) == Some(0x000a))
    {
        let Some(reference) = property(operation, 0x0b01).and_then(weak_key) else {
            continue;
        };
        let Some(definition) = definitions.get(reference).copied() else {
            findings.push(format!(
                "{} references an OperationDefinition absent from Dictionary",
                operation.path.display()
            ));
            continue;
        };
        if let (Some(actual), Some(declared)) = (
            component_data_definition(operation),
            property(definition, 0x1e01)
                .and_then(weak_key)
                .and_then(|value| auid_string(value).ok()),
        ) {
            if actual != declared {
                findings.push(format!(
                    "{} DataDefinition differs from {}",
                    operation.path.display(),
                    definition.path.display()
                ));
            }
        }
        let transition = ancestor_with_class(operation, by_path, 0x0017).is_some();
        let inputs = references
            .get(&(operation.path.clone(), 0x0b02))
            .map_or(0, Vec::len);
        if let Some(expected) = property(definition, 0x1e07).and_then(signed_i32) {
            if expected >= 0
                && ((!transition && inputs != expected as usize) || (transition && expected != 2))
            {
                findings.push(format!(
                    "{} has {inputs} explicit inputs but {} declares NumberInputs {expected}",
                    operation.path.display(),
                    definition.path.display()
                ));
            }
            if let Some(bypass) = property(operation, 0x0b04)
                .and_then(|value| read_u32(&value.data, 0).filter(|_| value.data.len() == 4))
            {
                if bypass == 0 || expected >= 0 && bypass > expected as u32 {
                    findings.push(format!(
                        "{} BypassOverride {bypass} is outside its input range",
                        operation.path.display()
                    ));
                }
            }
        }
        if let Ok(operation_id) = auid_string(reference) {
            let transition_only = matches!(
                operation_id.as_str(),
                "0c3bea40-fc05-11d2-8a29-0050040ef7d2"
                    | "0c3bea44-fc05-11d2-8a29-0050040ef7d2"
                    | "0c3bea41-fc05-11d2-8a29-0050040ef7d2"
                    | "2311bd90-b5da-4285-aa3a-8552848779b3"
            );
            let segment_only = matches!(
                operation_id.as_str(),
                "9d2ea890-0968-11d3-8a38-0050040ef7d2"
                    | "9d2ea891-0968-11d3-8a38-0050040ef7d2"
                    | "9d2ea894-0968-11d3-8a38-0050040ef7d2"
                    | "9d2ea893-0968-11d3-8a38-0050040ef7d2"
            );
            if transition_only && !transition {
                findings.push(format!(
                    "{} invokes transition-only operation {operation_id} outside Transition",
                    operation.path.display()
                ));
            }
            if segment_only && transition {
                findings.push(format!(
                    "{} invokes non-transition operation {operation_id} inside Transition",
                    operation.path.display()
                ));
            }
        }
    }
}

fn ancestor_with_class(
    object: &StoredObject,
    by_path: &HashMap<&Path, &StoredObject>,
    code: u16,
) -> Option<PathBuf> {
    let mut path = object.path.parent();
    while let Some(candidate) = path {
        if by_path
            .get(candidate)
            .is_some_and(|value| class_code(&value.class_id) == Some(code))
        {
            return Some(candidate.to_path_buf());
        }
        path = candidate.parent();
    }
    None
}

fn validate_scope_references(
    objects: &[StoredObject],
    by_path: &HashMap<&Path, &StoredObject>,
    references: &HashMap<(PathBuf, u16), Vec<PathBuf>>,
    findings: &mut Findings,
) {
    for reference in objects
        .iter()
        .filter(|object| class_code(&object.class_id) == Some(0x000d))
    {
        let Some(relative_scope) =
            property(reference, 0x0e01).and_then(|value| read_u32(&value.data, 0))
        else {
            continue;
        };
        let Some(relative_slot) =
            property(reference, 0x0e02).and_then(|value| read_u32(&value.data, 0))
        else {
            continue;
        };
        let mut scopes = Vec::new();
        let mut mob = None;
        let mut ancestor = reference.path.parent();
        while let Some(path) = ancestor {
            match by_path
                .get(path)
                .and_then(|object| class_code(&object.class_id))
            {
                Some(0x000b) => scopes.push(path.to_path_buf()),
                Some(0x0034..=0x0037) => {
                    mob = Some(path.to_path_buf());
                    break;
                }
                _ => {}
            }
            ancestor = path.parent();
        }
        let target = usize::try_from(relative_scope).ok().and_then(|index| {
            scopes
                .get(index)
                .cloned()
                .or_else(|| (index == scopes.len()).then(|| mob.clone()).flatten())
        });
        let count = target.as_ref().and_then(|path| {
            by_path
                .get(path.as_path())
                .and_then(|object| match class_code(&object.class_id) {
                    Some(0x000b) => references.get(&(path.clone(), 0x0c01)).map(Vec::len),
                    Some(0x0034..=0x0037) => references.get(&(path.clone(), 0x4403)).map(Vec::len),
                    _ => None,
                })
        });
        if target.is_none()
            || count.is_none()
            || usize::try_from(relative_slot)
                .ok()
                .is_none_or(|slot| slot >= count.unwrap())
        {
            findings.push(format!(
                "{} ScopeReference ({relative_scope}, {relative_slot}) does not resolve to an existing track",
                reference.path.display()
            ));
        }
    }
}

fn validate_edit_protocol(
    objects: &[StoredObject],
    references: &HashMap<(PathBuf, u16), Vec<PathBuf>>,
    summary: &EditSummary,
    findings: &mut Findings,
    warnings: &mut Findings,
) {
    let protocol_claimed = if let Some(header) = objects
        .iter()
        .find(|object| class_code(&object.class_id) == Some(0x002f))
    {
        match property(header, 0x3b09).and_then(direct_auid) {
            Some(value) if value == EDIT_PROTOCOL => true,
            Some(value) => {
                warnings.push(format!(
                    "{} OperationalPattern is {value}; Edit Protocol-only rules were not applied",
                    header.path.display()
                ));
                false
            }
            None => {
                warnings.push(format!(
                    "{} does not label the optional Edit Protocol operational pattern; protocol-only rules were not applied",
                    header.path.display()
                ));
                false
            }
        }
    } else {
        false
    };
    if !protocol_claimed {
        return;
    }

    if let Some((first_path, first_rate)) = summary.audio_rates.first() {
        for (path, rate) in summary.audio_rates.iter().skip(1) {
            if !first_rate.equal(*rate) {
                findings.push(format!(
                    "audio EditRate differs between {} and {}",
                    first_path.display(),
                    path.display()
                ));
            }
        }
    }
    validate_protocol_sample_rates(objects, references, findings);

    let mut top_names = HashSet::new();
    let referenced_mobs: HashSet<&Path> = summary
        .source_edges
        .iter()
        .map(|(_, target)| target.as_path())
        .collect();
    for mob in objects
        .iter()
        .filter(|object| matches!(class_code(&object.class_id), Some(0x0035..=0x0037)))
    {
        let slots = references
            .get(&(mob.path.clone(), 0x4403))
            .cloned()
            .unwrap_or_default();
        let mut essence_names = HashSet::new();
        for slot_path in &slots {
            let Some(slot) = objects.iter().find(|object| object.path == *slot_path) else {
                continue;
            };
            let segment = references
                .get(&(slot.path.clone(), 0x4803))
                .and_then(|targets| targets.first())
                .and_then(|path| objects.iter().find(|object| object.path == *path));
            let essence = segment
                .and_then(component_data_definition)
                .as_deref()
                .is_some_and(is_essence_definition);
            if !essence {
                continue;
            }
            match property(slot, 0x4802).and_then(|value| utf16_string(&value.data)) {
                Some(name) if !name.trim().is_empty() => {
                    if !essence_names.insert(name.clone()) {
                        findings.push(format!(
                            "{} repeats essence SlotName {name:?}",
                            mob.path.display()
                        ));
                    }
                }
                _ => findings.push(format!(
                    "{} Edit Protocol essence track has no valid SlotName",
                    slot.path.display()
                )),
            }
            let requires_physical = matches!(class_code(&mob.class_id), Some(0x0035 | 0x0036))
                || class_code(&mob.class_id) == Some(0x0037)
                    && objects
                        .iter()
                        .find(|object| object.path == mob.path.join("EssenceDescription-4701"))
                        .is_none_or(|descriptor| {
                            !matches!(class_code(&descriptor.class_id), Some(0x0025..=0x002c))
                        });
            if requires_physical
                && property(slot, 0x4804)
                    .and_then(|value| read_u32(&value.data, 0))
                    .is_none()
            {
                findings.push(format!(
                    "{} Edit Protocol essence track requires PhysicalTrackNumber",
                    slot.path.display()
                ));
            }
        }
    }
    for mob in objects
        .iter()
        .filter(|object| class_code(&object.class_id) == Some(0x0035))
    {
        let usage = property(mob, 0x4408).and_then(direct_auid);
        let slots = references
            .get(&(mob.path.clone(), 0x4403))
            .cloned()
            .unwrap_or_default();
        match usage.as_deref() {
            Some(USAGE_TOP_LEVEL) => {
                if referenced_mobs.contains(mob.path.as_path()) {
                    findings.push(format!(
                        "{} top-level CompositionMob is referenced by another Mob",
                        mob.path.display()
                    ));
                }
                let name = property(mob, 0x4402).and_then(|value| utf16_string(&value.data));
                if name.as_deref().is_none_or(|value| value.trim().is_empty()) {
                    findings.push(format!(
                        "{} top-level CompositionMob has no valid Name",
                        mob.path.display()
                    ));
                } else if !top_names.insert(name.unwrap()) {
                    findings.push("top-level CompositionMob names are not unique");
                }
                let mut primary_timecode = false;
                for slot_path in slots {
                    let slot = objects.iter().find(|object| object.path == slot_path);
                    let segment = slot
                        .and_then(|slot| references.get(&(slot.path.clone(), 0x4803)))
                        .and_then(|targets| targets.first())
                        .and_then(|path| objects.iter().find(|object| object.path == *path));
                    if segment
                        .and_then(component_data_definition)
                        .as_deref()
                        == Some(DATADEF_TIMECODE)
                        && slot
                            .and_then(|slot| property(slot, 0x4804))
                            .and_then(|value| read_u32(&value.data, 0))
                            == Some(1)
                    {
                        primary_timecode = true;
                    }
                }
                if !primary_timecode {
                    findings.push(format!(
                        "{} has no primary timecode track with PhysicalTrackNumber 1",
                        mob.path.display()
                    ));
                }
            }
            Some(USAGE_LOWER_LEVEL) if !referenced_mobs.contains(mob.path.as_path()) => {
                findings.push(format!(
                    "{} lower-level CompositionMob is not referenced",
                    mob.path.display()
                ));
            }
            Some(USAGE_SUBCLIP) => {
                for slot in slots {
                    if !slot_has_single_class(objects, references, &slot, 0x0011) {
                        findings.push(format!(
                            "{} sub-clip essence slot does not contain exactly one SourceClip",
                            slot.display()
                        ));
                    }
                }
            }
            Some(USAGE_ADJUSTED_CLIP) => {
                for slot in slots {
                    if !slot_has_single_class(objects, references, &slot, 0x000a) {
                        findings.push(format!(
                            "{} adjusted-clip slot does not contain exactly one OperationGroup",
                            slot.display()
                        ));
                    }
                }
            }
            Some(other) => warnings.push(format!(
                "{} uses CompositionMob UsageCode {other} outside the supported Edit Protocol roles",
                mob.path.display()
            )),
            None => warnings.push(format!(
                "{} CompositionMob omits UsageCode and is treated as lower-level material",
                mob.path.display()
            )),
        }
    }

    for mob in objects
        .iter()
        .filter(|object| class_code(&object.class_id) == Some(0x0036))
    {
        if property(mob, 0x4408).and_then(direct_auid).as_deref() == Some(USAGE_TEMPLATE) {
            for slot in references
                .get(&(mob.path.clone(), 0x4403))
                .into_iter()
                .flatten()
            {
                if !slot_has_zero_source_clip(objects, references, slot) {
                    findings.push(format!(
                        "{} template clip slot must contain one zero-value SourceClip",
                        slot.display()
                    ));
                }
            }
        }
    }

    validate_file_descriptors(objects, findings, warnings);
    validate_locators(objects, findings);
}

fn validate_protocol_sample_rates(
    objects: &[StoredObject],
    references: &HashMap<(PathBuf, u16), Vec<PathBuf>>,
    findings: &mut Findings,
) {
    let mut audio_sample_rates = Vec::new();
    for mob in objects
        .iter()
        .filter(|object| class_code(&object.class_id) == Some(0x0037))
    {
        let descriptor = objects
            .iter()
            .find(|object| object.path == mob.path.join("EssenceDescription-4701"));
        let sample_rate = descriptor
            .and_then(|descriptor| property(descriptor, 0x3001))
            .and_then(rational);
        for slot_path in references
            .get(&(mob.path.clone(), 0x4403))
            .into_iter()
            .flatten()
        {
            let slot = objects.iter().find(|object| object.path == *slot_path);
            let segment = slot
                .and_then(|slot| references.get(&(slot.path.clone(), 0x4803)))
                .and_then(|targets| targets.first())
                .and_then(|path| objects.iter().find(|object| object.path == *path));
            let definition = segment.and_then(component_data_definition);
            let edit_rate = slot
                .and_then(|slot| property(slot, 0x4b01))
                .and_then(rational);
            if definition.as_deref().is_some_and(is_sound_definition) {
                if let (Some(descriptor), Some(sample_rate)) = (descriptor, sample_rate) {
                    audio_sample_rates.push((descriptor.path.clone(), sample_rate));
                }
            } else if definition
                .as_deref()
                .is_some_and(|value| matches!(value, DATADEF_PICTURE | DATADEF_LEGACY_PICTURE))
            {
                if let (Some(edit_rate), Some(sample_rate)) = (edit_rate, sample_rate) {
                    if !edit_rate.less_or_equal(sample_rate) {
                        findings.push(format!(
                            "{} video EditRate exceeds descriptor SampleRate",
                            slot_path.display()
                        ));
                    }
                }
            }
        }
    }
    if let Some((first_path, first_rate)) = audio_sample_rates.first() {
        for (path, rate) in audio_sample_rates.iter().skip(1) {
            if !first_rate.equal(*rate) {
                findings.push(format!(
                    "audio FileDescriptor SampleRate differs between {} and {}",
                    first_path.display(),
                    path.display()
                ));
            }
        }
    }
}

fn slot_has_single_class(
    objects: &[StoredObject],
    references: &HashMap<(PathBuf, u16), Vec<PathBuf>>,
    slot: &Path,
    code: u16,
) -> bool {
    let Some(segment) = references
        .get(&(slot.to_path_buf(), 0x4803))
        .and_then(|targets| targets.first())
        .and_then(|path| objects.iter().find(|object| object.path == *path))
    else {
        return false;
    };
    if class_code(&segment.class_id) == Some(code) {
        return true;
    }
    class_code(&segment.class_id) == Some(0x000f)
        && references
            .get(&(segment.path.clone(), 0x1001))
            .is_some_and(|targets| {
                targets.len() == 1
                    && targets.first().is_some_and(|path| {
                        objects
                            .iter()
                            .find(|object| object.path == *path)
                            .is_some_and(|object| class_code(&object.class_id) == Some(code))
                    })
            })
}

fn slot_has_zero_source_clip(
    objects: &[StoredObject],
    references: &HashMap<(PathBuf, u16), Vec<PathBuf>>,
    slot: &Path,
) -> bool {
    let segment = references
        .get(&(slot.to_path_buf(), 0x4803))
        .and_then(|targets| targets.first())
        .and_then(|path| objects.iter().find(|object| object.path == *path));
    let clip = segment.filter(|object| class_code(&object.class_id) == Some(0x0011));
    clip.is_some_and(|clip| {
        property(clip, 0x1101).is_none_or(|value| value.data.iter().all(|byte| *byte == 0))
    })
}

fn validate_file_descriptors(
    objects: &[StoredObject],
    findings: &mut Findings,
    warnings: &mut Findings,
) {
    for mob in objects
        .iter()
        .filter(|object| class_code(&object.class_id) == Some(0x0037))
    {
        let descriptor_path = mob.path.join("EssenceDescription-4701");
        let Some(descriptor) = objects.iter().find(|object| object.path == descriptor_path) else {
            continue;
        };
        if matches!(class_code(&descriptor.class_id), Some(0x0025..=0x002c)) {
            let sample_rate = property(descriptor, 0x3001).and_then(rational);
            if !sample_rate.is_some_and(Rational::positive) {
                findings.push(format!(
                    "{} has invalid FileDescriptor SampleRate",
                    descriptor.path.display()
                ));
            }
            if property(descriptor, 0x3002)
                .and_then(signed_i64)
                .is_some_and(|length| length < 0)
            {
                findings.push(format!(
                    "{} has negative FileDescriptor Length",
                    descriptor.path.display()
                ));
            }
            if property(descriptor, 0x3004).is_none() {
                warnings.push(format!(
                    "{} file source omits ContainerFormat",
                    descriptor.path.display()
                ));
            }
        }
    }
}

fn validate_locators(objects: &[StoredObject], findings: &mut Findings) {
    for locator in objects
        .iter()
        .filter(|object| class_code(&object.class_id) == Some(0x0032))
    {
        let Some(value) = property(locator, 0x4001).and_then(|value| utf16_string(&value.data))
        else {
            continue;
        };
        let absolute = Url::parse(&value);
        let valid = match absolute {
            Ok(url) => url.scheme() == "file",
            Err(_) => Url::parse("file:///")
                .ok()
                .and_then(|base| base.join(&value).ok())
                .is_some(),
        };
        if !valid {
            findings.push(format!(
                "{} has a non-file or malformed NetworkLocator URI",
                locator.path.display()
            ));
        }
    }
}

fn component_data_definition(object: &StoredObject) -> Option<String> {
    property(object, 0x0201)
        .and_then(weak_key)
        .and_then(|bytes| auid_string(bytes).ok())
}

fn is_sound_definition(value: &str) -> bool {
    matches!(value, DATADEF_SOUND | DATADEF_LEGACY_SOUND)
}

fn is_essence_definition(value: &str) -> bool {
    matches!(
        value,
        DATADEF_SOUND | DATADEF_LEGACY_SOUND | DATADEF_PICTURE | DATADEF_LEGACY_PICTURE
    )
}

fn weak_key(property: &StoredProperty) -> Option<&[u8]> {
    if !matches!(property.format, SF_WEAK_REF | SF_WEAK_VECTOR | SF_WEAK_SET)
        || property.data.len() < 5
    {
        return None;
    }
    let size = usize::from(property.data[4]);
    matches!(size, 16 | 32)
        .then_some(())
        .and_then(|()| property.data.get(5..5 + size))
        .filter(|_| property.data.len() == 5 + size)
}

fn direct_auid(property: &StoredProperty) -> Option<String> {
    (property.data.len() == 16)
        .then(|| auid_string(&property.data).ok())
        .flatten()
}

fn auid_string(bytes: &[u8]) -> Result<String, ()> {
    let value: [u8; 16] = bytes.try_into().map_err(|_| ())?;
    Ok(format!(
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

fn rational(property: &StoredProperty) -> Option<Rational> {
    if property.data.len() != 8 {
        return None;
    }
    let numerator = i32::from_le_bytes(property.data[0..4].try_into().ok()?);
    let denominator = i32::from_le_bytes(property.data[4..8].try_into().ok()?);
    (denominator != 0).then_some(Rational {
        numerator,
        denominator,
    })
}

fn signed_i64(property: &StoredProperty) -> Option<i64> {
    let bytes: [u8; 8] = property.data.as_slice().try_into().ok()?;
    Some(i64::from_le_bytes(bytes))
}

fn signed_i32(property: &StoredProperty) -> Option<i32> {
    let bytes: [u8; 4] = property.data.as_slice().try_into().ok()?;
    Some(i32::from_le_bytes(bytes))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn utf16_string(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 || !bytes.len().is_multiple_of(2) || !bytes.ends_with(&[0, 0]) {
        return None;
    }
    let mut units = Vec::with_capacity(bytes.len() / 2 - 1);
    for pair in bytes.chunks_exact(2) {
        let unit = u16::from_le_bytes([pair[0], pair[1]]);
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    String::from_utf16(&units).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_class_code() {
        assert_eq!(
            class_code("0d010101-0101-3500-060e-2b3402060101"),
            Some(0x35)
        );
        assert_eq!(class_code("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"), None);
    }

    #[test]
    fn parses_vector_and_set_indexes() {
        let object = StoredObject {
            path: PathBuf::from("/object"),
            class_id: String::new(),
            properties: Vec::new(),
        };
        let mut streams = HashMap::new();
        streams.insert(
            PathBuf::from("/object/Vector index"),
            [2_u32, 3, u32::MAX, 0, 2]
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect(),
        );
        let mut set = Vec::new();
        set.extend_from_slice(&1_u32.to_le_bytes());
        set.extend_from_slice(&2_u32.to_le_bytes());
        set.extend_from_slice(&u32::MAX.to_le_bytes());
        set.extend_from_slice(&0x4401_u16.to_le_bytes());
        set.push(32);
        set.extend_from_slice(&4_u32.to_le_bytes());
        set.extend_from_slice(&1_u32.to_le_bytes());
        set.extend_from_slice(&[7; 32]);
        streams.insert(PathBuf::from("/object/Set index"), set);
        assert_eq!(
            parse_vector_index(&object, "Vector", &streams).unwrap(),
            [0, 2]
        );
        assert_eq!(parse_set_index(&object, "Set", &streams).unwrap(), [4]);
    }

    #[test]
    fn rejects_non_file_absolute_locator() {
        let object = StoredObject {
            path: PathBuf::from("/locator"),
            class_id: "0d010101-0101-3200-060e-2b3402060101".to_owned(),
            properties: vec![StoredProperty {
                pid: 0x4001,
                format: SF_DATA,
                data: "https://example.test/media.wav"
                    .encode_utf16()
                    .chain([0])
                    .flat_map(u16::to_le_bytes)
                    .collect(),
            }],
        };
        let mut findings = Findings::default();
        validate_locators(&[object], &mut findings);
        assert_eq!(findings.total, 1);
    }

    #[test]
    fn reports_missing_inherited_header_properties() {
        let header_name: Vec<u8> = "Header-2"
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect();
        let objects = vec![
            StoredObject {
                path: PathBuf::from("/"),
                class_id: String::new(),
                properties: vec![StoredProperty {
                    pid: 2,
                    format: SF_STRONG_REF,
                    data: header_name,
                }],
            },
            StoredObject {
                path: PathBuf::from("/Header-2"),
                class_id: "0d010101-0101-2f00-060e-2b3402060101".to_owned(),
                properties: Vec::new(),
            },
        ];
        let audit = audit(&objects, &HashMap::new(), &HashSet::new());
        let required = audit
            .checks
            .iter()
            .find(|check| check.rule_id == "FORGE-AAF-OBJECT-REQUIRED-PROPERTIES")
            .unwrap();
        assert!(!required.passed);
        assert_eq!(required.observed.as_ref().unwrap()["total"], 6);
    }

    #[test]
    fn reports_sequence_length_mismatch() {
        let weak_definition = StoredProperty {
            pid: 0x0201,
            format: SF_WEAK_REF,
            data: [
                &[0, 0, 0, 0, 16][..],
                &[0x02, 0x02, 0x03, 0x01, 0x00, 0x01, 0x00, 0x00][..],
                &[0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x01][..],
            ]
            .concat(),
        };
        let sequence = StoredObject {
            path: PathBuf::from("/sequence"),
            class_id: "0d010101-0101-0f00-060e-2b3402060101".to_owned(),
            properties: vec![
                weak_definition.clone(),
                StoredProperty {
                    pid: 0x0202,
                    format: SF_DATA,
                    data: 10_i64.to_le_bytes().to_vec(),
                },
            ],
        };
        let filler = StoredObject {
            path: PathBuf::from("/sequence/components{0}"),
            class_id: "0d010101-0101-0900-060e-2b3402060101".to_owned(),
            properties: vec![
                weak_definition,
                StoredProperty {
                    pid: 0x0202,
                    format: SF_DATA,
                    data: 4_i64.to_le_bytes().to_vec(),
                },
            ],
        };
        let by_path = HashMap::from([
            (sequence.path.as_path(), &sequence),
            (filler.path.as_path(), &filler),
        ]);
        let references =
            HashMap::from([((sequence.path.clone(), 0x1001), vec![filler.path.clone()])]);
        let mut timeline = Findings::default();
        let mut effects = Findings::default();
        validate_component(
            &sequence,
            component_data_definition(&sequence).as_deref(),
            &by_path,
            &references,
            &mut timeline,
            &mut effects,
            &mut HashSet::new(),
        );
        assert_eq!(timeline.total, 1);
        assert!(timeline.values[0].contains("calculate to 4"));
    }
}
