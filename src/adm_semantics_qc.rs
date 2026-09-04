//! Bounded semantic audit for ADM content and presentation metadata.
//!
//! The audit covers machine-checkable semantics that are deliberately outside
//! the renderer-facing presentation and interactivity audits: dialogue kind
//! enumerations, alternative-value-set selection references, default
//! programme selection, presentation intent, importance metadata, and the
//! non-authoritative role of `tagList` values.

use crate::metadata;
use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;
pub const REPORT_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/adm-semantics-report-v1";
pub const VALIDATOR: &str = "forge-adm-semantics-qc-1";
pub const ADM_STANDARD: &str = "ITU-R BS.2076-3";
pub const USAGE_GUIDELINE: &str = "Report ITU-R BS.2388-7";

pub const DEFAULT_MAX_PROGRAMMES: usize = 4096;
pub const HARD_MAX_PROGRAMMES: usize = 65_535;
pub const DEFAULT_MAX_CONTENTS: usize = 4096;
pub const HARD_MAX_CONTENTS: usize = 65_535;
pub const DEFAULT_MAX_OBJECTS: usize = 4096;
pub const HARD_MAX_OBJECTS: usize = 65_535;
pub const DEFAULT_MAX_REPORT_ITEMS: usize = 32_768;
pub const HARD_MAX_REPORT_ITEMS: usize = 250_000;
pub const DEFAULT_MAX_AXML_BYTES: usize = 16 * 1024 * 1024;
pub const HARD_MAX_AXML_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_MAX_XML_NODES: usize = 250_000;
pub const HARD_MAX_XML_NODES: usize = 1_000_000;

const SCOPE_NOTE: &str = "Metadata semantics only: this report does not render audio, prove a renderer's capacity, or establish loudness or true-peak compliance.";
const IMPORTANCE_PLAN_NOTE: &str = "Object-count planning uses only explicit audioObject importance values. Objects with missing or invalid importance and objects at importance 10 remain protected; nested rendering, pack quality, track count, merging, and audio results require a renderer-aware stage.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PresentationIntent {
    Auto,
    Fixed,
    Interactive,
}

impl PresentationIntent {
    fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Fixed => "fixed",
            Self::Interactive => "interactive",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    pub input: PathBuf,
    pub presentation_intent: PresentationIntent,
    pub expected_default_programme: Option<String>,
    pub renderer_object_limit: Option<usize>,
    pub max_programmes: usize,
    pub max_contents: usize,
    pub max_objects: usize,
    pub max_report_items: usize,
    pub max_axml_bytes: usize,
    pub max_xml_nodes: usize,
}

#[derive(Debug, Serialize)]
pub struct Limits {
    pub max_programmes: usize,
    pub max_contents: usize,
    pub max_objects: usize,
    pub max_report_items: usize,
    pub max_axml_bytes: usize,
    pub max_xml_nodes: usize,
}

#[derive(Debug, Serialize)]
pub struct Counts {
    pub programmes: usize,
    pub contents: usize,
    pub objects: usize,
    pub pack_formats: usize,
    pub block_formats: usize,
    pub alternative_value_sets: usize,
    pub alternative_value_set_references: usize,
    pub complementary_object_references: usize,
    pub tag_groups: usize,
    pub tags: usize,
    pub xml_nodes: usize,
    pub report_items: usize,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub schema_version: u32,
    pub validator: &'static str,
    pub adm_standard: &'static str,
    pub usage_guideline: &'static str,
    pub input_path: String,
    pub input_bytes: u64,
    pub input_sha256: String,
    pub axml_bytes: usize,
    pub limits: Limits,
    pub counts: Counts,
    pub requested_presentation_intent: &'static str,
    pub inferred_presentation_mode: &'static str,
    pub expected_default_programme_id: Option<String>,
    pub default_programme_id: Option<String>,
    pub normative_passed: bool,
    pub requested_policy_passed: bool,
    pub rendered_audio_verified: bool,
    pub renderer_capacity_verified: bool,
    pub tag_semantics_authoritative: bool,
    pub scope_note: &'static str,
    pub passed: bool,
    pub dialogue_contents: Vec<DialogueAudit>,
    pub alternative_value_set_references: Vec<AvsReferenceAudit>,
    pub importance: ImportanceReport,
    pub tag_groups: Vec<TagGroupAudit>,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Serialize)]
pub struct DialogueAudit {
    pub content_id: String,
    pub content_name: String,
    pub dialogue_present: bool,
    pub raw_value: Option<String>,
    pub dialogue_value: Option<u8>,
    pub kind_attribute: Option<String>,
    pub raw_kind_value: Option<String>,
    pub kind_value: Option<u8>,
    pub content_kind: Option<&'static str>,
    pub passed: bool,
}

#[derive(Debug, Serialize)]
pub struct AvsReferenceAudit {
    pub parent_element: &'static str,
    pub parent_id: String,
    pub parent_name: String,
    pub unique_object_groups: bool,
    pub references_resolve_to_owner_objects: bool,
    pub passed: bool,
    pub references: Vec<AvsReference>,
}

#[derive(Debug, Serialize)]
pub struct AvsReference {
    pub id: String,
    pub object_key: Option<String>,
    pub owner_object_id: Option<String>,
    pub resolves_locally: bool,
    pub owner_matches_id: bool,
}

#[derive(Debug, Serialize)]
pub struct ImportanceReport {
    pub audio_object_explicit: usize,
    pub audio_object_unspecified: usize,
    pub audio_pack_format_explicit: usize,
    pub audio_pack_format_unspecified: usize,
    pub audio_block_format_explicit: usize,
    pub audio_block_format_unspecified: usize,
    pub invalid_values: usize,
    pub audio_block_format_values_are_informational: bool,
    pub entries: Vec<ImportanceEntry>,
    pub object_threshold_plan: Option<ObjectThresholdPlan>,
}

#[derive(Debug, Serialize)]
pub struct ImportanceEntry {
    pub element: &'static str,
    pub element_id: Option<String>,
    pub element_name: Option<String>,
    pub raw_value: String,
    pub value: Option<u8>,
    pub semantic_effect: &'static str,
    pub recommended_use: &'static str,
    pub valid: bool,
}

#[derive(Debug, Serialize)]
pub struct ObjectThresholdPlan {
    pub target_object_count: usize,
    pub starting_object_count: usize,
    pub achievable_without_protected_object_discard: bool,
    pub selected_threshold: Option<u8>,
    pub resulting_object_count: usize,
    pub protected_importance_10: usize,
    pub protected_unspecified_or_invalid: usize,
    pub discard_candidates: Vec<String>,
    pub steps: Vec<ThresholdStep>,
    pub requires_renderer_or_merge: bool,
    pub note: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ThresholdStep {
    pub threshold: u8,
    pub retained_object_count: usize,
    pub discard_candidate_count: usize,
}

#[derive(Debug, Serialize)]
pub struct TagGroupAudit {
    pub index: usize,
    pub tags: Vec<TagValue>,
    pub references: Vec<TagReference>,
    pub semantic_authority: bool,
    pub passed: bool,
}

#[derive(Debug, Serialize)]
pub struct TagValue {
    pub class: Option<String>,
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct TagReference {
    pub element: &'static str,
    pub id: String,
    pub resolves_locally: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuleClassification {
    Normative,
    Guidance,
    Policy,
    Informational,
}

#[derive(Debug, Serialize)]
pub struct Rule {
    pub rule_id: &'static str,
    pub authority: &'static str,
    pub section: &'static str,
    pub classification: RuleClassification,
    pub subject: String,
    pub requirement: String,
    pub observed: String,
    pub enforced: bool,
    pub passed: bool,
}

#[derive(Debug, Default)]
struct ParsedXml {
    nodes: Vec<Node>,
}

#[derive(Debug, Default)]
struct Node {
    name: String,
    parent: Option<usize>,
    attributes: HashMap<String, String>,
    text: String,
}

#[derive(Debug, Clone)]
struct ObjectImportance {
    id: String,
    value: Option<u8>,
    valid: bool,
}

pub fn run(options: &Options) -> Result<Report, String> {
    validate_options(options)?;
    let input = fs::canonicalize(&options.input)
        .map_err(|error| format!("resolve ADM input {}: {error}", options.input.display()))?;
    ensure_regular_file(&input)?;
    let (input_sha256, input_bytes) = sha256_file(&input)?;
    let axml = metadata::read_wave_chunk_limited(&input, *b"axml", options.max_axml_bytes)?
        .ok_or_else(|| "ADM semantics QC requires an axml chunk".to_string())?;
    if metadata::read_wave_chunk_limited(&input, *b"chna", options.max_axml_bytes)?.is_none() {
        return Err("ADM semantics QC requires a chna chunk".into());
    }
    let parsed = parse_xml(&axml, options.max_xml_nodes)?;

    let programmes = definitions(&parsed, "audioProgramme", "audioProgrammeID")?;
    let contents = definitions(&parsed, "audioContent", "audioContentID")?;
    let objects = definitions(&parsed, "audioObject", "audioObjectID")?;
    enforce_count("audioProgramme", programmes.len(), options.max_programmes)?;
    enforce_count("audioContent", contents.len(), options.max_contents)?;
    enforce_count("audioObject", objects.len(), options.max_objects)?;

    let mut rules = Vec::new();
    let dialogue_contents = audit_dialogue(&parsed, &contents, &mut rules);
    let (alternative_value_set_references, avs_reference_count) =
        audit_avs_references(&parsed, &programmes, &contents, &mut rules)?;

    let complementary_object_references = count_nodes(&parsed, "audioComplementaryObjectIDRef");
    let inferred_presentation_mode =
        inferred_presentation_mode(programmes.len(), complementary_object_references);
    audit_presentation_intent(
        options.presentation_intent,
        programmes.len(),
        complementary_object_references,
        inferred_presentation_mode,
        &mut rules,
    );

    let default_programme_id = audit_default_programme(
        &programmes,
        options.expected_default_programme.as_deref(),
        &mut rules,
    );
    let importance = audit_importance(&parsed, &objects, options.renderer_object_limit, &mut rules);
    let tag_groups = audit_tag_groups(&parsed, &programmes, &contents, &objects, &mut rules);

    push_rule(
        &mut rules,
        "BS2076-3-TAG-NON-AUTHORITATIVE",
        ADM_STANDARD,
        "§ 5.11",
        RuleClassification::Informational,
        "/audioFormatExtended/tagList",
        "tag values supplement metadata and shall not change ADM parsing or replace ADM elements",
        "Forge records tags as non-authoritative annotations",
        false,
        true,
    );
    push_rule(
        &mut rules,
        "BS2388-7-ID-CASE-INSENSITIVE",
        USAGE_GUIDELINE,
        "§ 3.2.2",
        RuleClassification::Informational,
        "/audioFormatExtended",
        "compare hexadecimal letters in ADM IDs without case sensitivity",
        "Forge rejects case-only duplicate definitions and resolves ID references case-insensitively",
        false,
        true,
    );
    push_rule(
        &mut rules,
        "FORGE-SEMANTICS-SCOPE",
        "Forge scope contract",
        "adm-semantics-report-v1",
        RuleClassification::Informational,
        input.display().to_string(),
        "metadata inspection shall not be represented as rendered-audio verification",
        SCOPE_NOTE,
        false,
        true,
    );

    let report_items = report_item_count(
        &dialogue_contents,
        &alternative_value_set_references,
        &importance,
        &tag_groups,
        &rules,
    )?;
    if report_items > options.max_report_items {
        return Err(format!(
            "ADM semantics report expands to {report_items} items, exceeding the configured limit {}",
            options.max_report_items
        ));
    }

    let normative_passed = rules
        .iter()
        .filter(|rule| rule.classification == RuleClassification::Normative)
        .all(|rule| rule.passed);
    let requested_policy_passed = rules
        .iter()
        .filter(|rule| rule.enforced && rule.classification != RuleClassification::Normative)
        .all(|rule| rule.passed);
    let passed = rules
        .iter()
        .filter(|rule| rule.enforced)
        .all(|rule| rule.passed);
    ensure_unchanged(&input, &input_sha256, input_bytes)?;

    let pack_formats = count_nodes(&parsed, "audioPackFormat");
    let block_formats = count_canonical_nodes(&parsed, "audioBlockFormat");
    let alternative_value_sets = count_nodes(&parsed, "alternativeValueSet");
    let tag_count = tag_groups.iter().map(|group| group.tags.len()).sum();

    Ok(Report {
        schema: REPORT_SCHEMA,
        schema_version: SCHEMA_VERSION,
        validator: VALIDATOR,
        adm_standard: ADM_STANDARD,
        usage_guideline: USAGE_GUIDELINE,
        input_path: input.display().to_string(),
        input_bytes,
        input_sha256,
        axml_bytes: axml.len(),
        limits: Limits {
            max_programmes: options.max_programmes,
            max_contents: options.max_contents,
            max_objects: options.max_objects,
            max_report_items: options.max_report_items,
            max_axml_bytes: options.max_axml_bytes,
            max_xml_nodes: options.max_xml_nodes,
        },
        counts: Counts {
            programmes: programmes.len(),
            contents: contents.len(),
            objects: objects.len(),
            pack_formats,
            block_formats,
            alternative_value_sets,
            alternative_value_set_references: avs_reference_count,
            complementary_object_references,
            tag_groups: tag_groups.len(),
            tags: tag_count,
            xml_nodes: parsed.nodes.len(),
            report_items,
        },
        requested_presentation_intent: options.presentation_intent.name(),
        inferred_presentation_mode,
        expected_default_programme_id: options.expected_default_programme.clone(),
        default_programme_id,
        normative_passed,
        requested_policy_passed,
        rendered_audio_verified: false,
        renderer_capacity_verified: false,
        tag_semantics_authoritative: false,
        scope_note: SCOPE_NOTE,
        passed,
        dialogue_contents,
        alternative_value_set_references,
        importance,
        tag_groups,
        rules,
    })
}

pub fn write_report(
    path: &Path,
    report: &Report,
    compact: bool,
    overwrite: bool,
) -> Result<(), String> {
    if path.exists() && !overwrite {
        return Err(format!(
            "refusing to replace existing ADM semantics report {}; pass --overwrite",
            path.display()
        ));
    }
    let mut bytes = if compact {
        serde_json::to_vec(report)
    } else {
        serde_json::to_vec_pretty(report)
    }
    .map_err(|error| format!("serialize ADM semantics report: {error}"))?;
    bytes.push(b'\n');
    let mut output = crate::atomic::AtomicOutput::new_with_overwrite(path, overwrite)?;
    output.write_all(&bytes)?;
    output.commit()
}

fn audit_dialogue(
    parsed: &ParsedXml,
    contents: &BTreeMap<String, usize>,
    rules: &mut Vec<Rule>,
) -> Vec<DialogueAudit> {
    let mut audits = Vec::with_capacity(contents.len());
    for (content_id, index) in contents {
        let node = &parsed.nodes[*index];
        let content_name = node
            .attributes
            .get("audioContentName")
            .cloned()
            .unwrap_or_else(|| content_id.clone());
        let dialogues = direct_children(parsed, *index, "dialogue");
        let cardinality_passed = dialogues.len() <= 1;
        push_rule(
            rules,
            "BS2076-3-DIALOGUE-CARDINALITY",
            ADM_STANDARD,
            "§ 5.7.2, Table A1-34",
            RuleClassification::Normative,
            content_id,
            "audioContent contains zero or one dialogue element",
            format!("{} dialogue element(s)", dialogues.len()),
            true,
            cardinality_passed,
        );

        let Some(dialogue_index) = dialogues.first().copied() else {
            audits.push(DialogueAudit {
                content_id: content_id.clone(),
                content_name,
                dialogue_present: false,
                raw_value: None,
                dialogue_value: None,
                kind_attribute: None,
                raw_kind_value: None,
                kind_value: None,
                content_kind: None,
                passed: cardinality_passed,
            });
            continue;
        };
        let dialogue = &parsed.nodes[dialogue_index];
        let raw_value = dialogue.text.trim().to_owned();
        let dialogue_value = raw_value.parse::<u8>().ok().filter(|value| *value <= 2);
        let value_passed = dialogue_value.is_some();
        push_rule(
            rules,
            "BS2076-3-DIALOGUE-VALUE",
            ADM_STANDARD,
            "§ 5.7.3, Table A1-35",
            RuleClassification::Normative,
            content_id,
            "dialogue value is 0 (none), 1 (pure), or 2 (mixed)",
            if raw_value.is_empty() {
                "empty".into()
            } else {
                raw_value.clone()
            },
            true,
            value_passed,
        );

        let expected_attribute = dialogue_value.map(expected_dialogue_attribute);
        let present_attributes = DIALOGUE_KIND_ATTRIBUTES
            .iter()
            .filter(|attribute| dialogue.attributes.contains_key(**attribute))
            .copied()
            .collect::<Vec<_>>();
        let attribute_passed =
            expected_attribute.is_some_and(|expected| present_attributes.as_slice() == [expected]);
        push_rule(
            rules,
            "BS2076-3-DIALOGUE-KIND-ATTRIBUTE",
            ADM_STANDARD,
            "§ 5.7.3, Table A1-35",
            RuleClassification::Normative,
            content_id,
            "dialogue carries exactly the content-kind attribute selected by its value",
            if present_attributes.is_empty() {
                "no dialogue kind attribute".into()
            } else {
                present_attributes.join(", ")
            },
            true,
            attribute_passed,
        );

        let kind_attribute = expected_attribute.map(str::to_owned);
        let raw_kind_value = expected_attribute
            .and_then(|attribute| dialogue.attributes.get(attribute))
            .cloned();
        let kind_value = raw_kind_value
            .as_deref()
            .and_then(|value| value.trim().parse::<u8>().ok());
        let content_kind = dialogue_value
            .zip(kind_value)
            .and_then(|(dialogue, kind)| dialogue_kind(dialogue, kind));
        let kind_passed = content_kind.is_some();
        push_rule(
            rules,
            "BS2076-3-DIALOGUE-KIND-VALUE",
            ADM_STANDARD,
            "§ 5.7.3, Table A1-36",
            RuleClassification::Normative,
            content_id,
            "dialogue kind is one of the enumerators defined for the selected dialogue value",
            raw_kind_value
                .clone()
                .unwrap_or_else(|| "not present".into()),
            true,
            kind_passed,
        );
        audits.push(DialogueAudit {
            content_id: content_id.clone(),
            content_name,
            dialogue_present: true,
            raw_value: Some(raw_value),
            dialogue_value,
            kind_attribute,
            raw_kind_value,
            kind_value,
            content_kind,
            passed: cardinality_passed && value_passed && attribute_passed && kind_passed,
        });
    }
    audits
}

const DIALOGUE_KIND_ATTRIBUTES: [&str; 3] = [
    "nonDialogueContentKind",
    "dialogueContentKind",
    "mixedContentKind",
];

fn expected_dialogue_attribute(value: u8) -> &'static str {
    match value {
        0 => "nonDialogueContentKind",
        1 => "dialogueContentKind",
        2 => "mixedContentKind",
        _ => unreachable!("validated dialogue value"),
    }
}

fn dialogue_kind(dialogue: u8, kind: u8) -> Option<&'static str> {
    match (dialogue, kind) {
        (0, 0) => Some("undefined"),
        (0, 1) => Some("music"),
        (0, 2) => Some("effects"),
        (0, 3) => Some("music-and-effects"),
        (1, 0) => Some("undefined"),
        (1, 1) => Some("storyline-dialogue"),
        (1, 2) => Some("voiceover"),
        (1, 3) => Some("spoken-subtitle"),
        (1, 4) => Some("audio-description-visually-impaired"),
        (1, 5) => Some("commentary"),
        (1, 6) => Some("emergency"),
        (2, 0) => Some("undefined"),
        (2, 1) => Some("complete-main"),
        (2, 2) => Some("mixed"),
        (2, 3) => Some("hearing-impaired"),
        (2, 4) => Some("complete-main-with-audio-description-visually-impaired"),
        _ => None,
    }
}

fn audit_avs_references(
    parsed: &ParsedXml,
    programmes: &BTreeMap<String, usize>,
    contents: &BTreeMap<String, usize>,
    rules: &mut Vec<Rule>,
) -> Result<(Vec<AvsReferenceAudit>, usize), String> {
    let alternatives = definitions(parsed, "alternativeValueSet", "alternativeValueSetID")?;
    let alternatives_by_key = alternatives
        .iter()
        .map(|(id, index)| (id.to_ascii_uppercase(), *index))
        .collect::<HashMap<_, _>>();
    let mut audits = Vec::new();
    let mut reference_count = 0_usize;
    for (element, id_attribute, name_attribute, definitions) in [
        (
            "audioProgramme",
            "audioProgrammeID",
            "audioProgrammeName",
            programmes,
        ),
        (
            "audioContent",
            "audioContentID",
            "audioContentName",
            contents,
        ),
    ] {
        for (parent_id, parent_index) in definitions {
            let reference_nodes =
                direct_children(parsed, *parent_index, "alternativeValueSetIDRef");
            if reference_nodes.is_empty() {
                continue;
            }
            reference_count = reference_count
                .checked_add(reference_nodes.len())
                .ok_or_else(|| "alternative-value-set reference count overflow".to_string())?;
            let parent = &parsed.nodes[*parent_index];
            debug_assert_eq!(parent.attributes.get(id_attribute), Some(parent_id));
            let parent_name = parent
                .attributes
                .get(name_attribute)
                .cloned()
                .unwrap_or_else(|| parent_id.clone());
            let mut references = Vec::with_capacity(reference_nodes.len());
            let mut keys = HashSet::new();
            let mut unique_object_groups = true;
            let mut syntax_passed = true;
            let mut owners_passed = true;
            for reference_node in reference_nodes {
                let reference_id = parsed.nodes[reference_node].text.trim().to_owned();
                let object_key = parse_avs_id(&reference_id).map(str::to_owned);
                syntax_passed &= object_key.is_some();
                if let Some(key) = object_key.as_deref() {
                    unique_object_groups &= keys.insert(key.to_ascii_uppercase());
                }
                let alternative_index = alternatives_by_key
                    .get(&reference_id.to_ascii_uppercase())
                    .copied();
                let owner_object_id = alternative_index.and_then(|index| {
                    parsed.nodes[index].parent.and_then(|parent_index| {
                        let owner = &parsed.nodes[parent_index];
                        (owner.name == "audioObject")
                            .then(|| owner.attributes.get("audioObjectID").cloned())
                            .flatten()
                    })
                });
                let owner_matches_id = object_key.as_deref().is_some_and(|key| {
                    owner_object_id
                        .as_deref()
                        .is_some_and(|owner| owner.eq_ignore_ascii_case(&format!("AO_{key}")))
                });
                let resolves_locally = alternative_index.is_some();
                owners_passed &= resolves_locally && owner_matches_id;
                references.push(AvsReference {
                    id: reference_id,
                    object_key,
                    owner_object_id,
                    resolves_locally,
                    owner_matches_id,
                });
            }
            push_rule(
                rules,
                "BS2076-3-AVS-REFERENCE-SYNTAX",
                ADM_STANDARD,
                if element == "audioProgramme" {
                    "§ 5.8.2, Table A1-42"
                } else {
                    "§ 5.7.2, Table A1-34"
                },
                RuleClassification::Normative,
                parent_id,
                "alternativeValueSetIDRef values use AVS_wwww_zzzz hexadecimal syntax",
                references
                    .iter()
                    .map(|reference| reference.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                true,
                syntax_passed,
            );
            push_rule(
                rules,
                "BS2076-3-AVS-ONE-PER-OBJECT",
                ADM_STANDARD,
                if element == "audioProgramme" {
                    "§ 5.8.2"
                } else {
                    "§ 5.7.2"
                },
                RuleClassification::Normative,
                parent_id,
                "a parent references at most one alternativeValueSet from the same audioObject",
                format!("{} reference(s)", references.len()),
                true,
                unique_object_groups,
            );
            push_rule(
                rules,
                "BS2076-3-AVS-LOCAL-OWNER",
                ADM_STANDARD,
                "§§ 5.6.5.1, 5.7.2, 5.8.2",
                RuleClassification::Normative,
                parent_id,
                "each referenced alternativeValueSet exists under the audioObject encoded by its wwww digits",
                format!(
                    "{} of {} reference(s) resolve to the encoded owner",
                    references
                        .iter()
                        .filter(|reference| reference.resolves_locally && reference.owner_matches_id)
                        .count(),
                    references.len()
                ),
                true,
                owners_passed,
            );
            audits.push(AvsReferenceAudit {
                parent_element: element,
                parent_id: parent_id.clone(),
                parent_name,
                unique_object_groups,
                references_resolve_to_owner_objects: owners_passed,
                passed: syntax_passed && unique_object_groups && owners_passed,
                references,
            });
        }
    }
    audits.sort_by(|left, right| {
        (left.parent_element, &left.parent_id).cmp(&(right.parent_element, &right.parent_id))
    });
    Ok((audits, reference_count))
}

fn parse_avs_id(id: &str) -> Option<&str> {
    let mut parts = id.split('_');
    let prefix = parts.next()?;
    let object = parts.next()?;
    let alternative = parts.next()?;
    if prefix == "AVS"
        && object.len() == 4
        && alternative.len() == 4
        && object.bytes().all(|byte| byte.is_ascii_hexdigit())
        && alternative.bytes().all(|byte| byte.is_ascii_hexdigit())
        && parts.next().is_none()
    {
        Some(object)
    } else {
        None
    }
}

fn inferred_presentation_mode(
    programme_count: usize,
    complementary_reference_count: usize,
) -> &'static str {
    match (programme_count, complementary_reference_count) {
        (0, _) => "no-programme",
        (1, 0) => "single-fixed",
        (1, _) => "interactive-complementary",
        (_, 0) => "fixed-programme-alternatives",
        (_, _) => "hybrid-multiple-programmes-and-complementary",
    }
}

fn audit_presentation_intent(
    intent: PresentationIntent,
    programme_count: usize,
    complementary_reference_count: usize,
    inferred: &'static str,
    rules: &mut Vec<Rule>,
) {
    let (enforced, passed, requirement) = match intent {
        PresentationIntent::Auto => (
            false,
            true,
            "infer the presentation pattern without asserting authoring intent",
        ),
        PresentationIntent::Fixed => (
            true,
            programme_count > 0 && complementary_reference_count == 0,
            "fixed presentation intent has at least one programme and does not use complementary-object user selection",
        ),
        PresentationIntent::Interactive => (
            true,
            programme_count == 1 && complementary_reference_count > 0,
            "interactive alternative selection uses one audioProgramme with complementary audioObjects",
        ),
    };
    push_rule(
        rules,
        "BS2388-7-PRESENTATION-INTENT",
        USAGE_GUIDELINE,
        "§ 3.11.1",
        RuleClassification::Guidance,
        "/audioFormatExtended",
        requirement,
        format!(
            "requested={}, inferred={inferred}, programmes={programme_count}, complementary references={complementary_reference_count}",
            intent.name()
        ),
        enforced,
        passed,
    );
}

fn audit_default_programme(
    programmes: &BTreeMap<String, usize>,
    expected: Option<&str>,
    rules: &mut Vec<Rule>,
) -> Option<String> {
    let mut parsed = programmes
        .keys()
        .map(|id| (id.clone(), parse_programme_id(id)))
        .collect::<Vec<_>>();
    let ids_valid = parsed.iter().all(|(_, value)| value.is_some());
    push_rule(
        rules,
        "BS2076-3-PROGRAMME-ID-FOR-SELECTION",
        ADM_STANDARD,
        "§ 6, Table A1-62",
        RuleClassification::Normative,
        "/audioFormatExtended/audioProgramme",
        "audioProgramme IDs use APR_wwww hexadecimal syntax so numeric selection is deterministic",
        if programmes.is_empty() {
            "no audioProgramme elements".into()
        } else {
            programmes.keys().cloned().collect::<Vec<_>>().join(", ")
        },
        true,
        ids_valid,
    );
    let default = if ids_valid {
        parsed.sort_by_key(|(_, value)| *value);
        parsed.first().map(|(id, _)| id.clone())
    } else {
        None
    };
    push_rule(
        rules,
        "BS2388-7-DEFAULT-PROGRAMME",
        USAGE_GUIDELINE,
        "§ 3.11.2",
        RuleClassification::Guidance,
        "/audioFormatExtended/audioProgramme",
        "when no other selection information is known, choose the lowest numeric audioProgrammeID",
        default.clone().unwrap_or_else(|| {
            if programmes.is_empty() {
                "not applicable: no audioProgramme".into()
            } else {
                "not resolved because an ID is invalid".into()
            }
        }),
        false,
        ids_valid,
    );
    if let Some(expected) = expected {
        let exists = programmes
            .keys()
            .any(|id| id.eq_ignore_ascii_case(expected));
        let matches = exists
            && default
                .as_deref()
                .is_some_and(|selected| selected.eq_ignore_ascii_case(expected));
        push_rule(
            rules,
            "FORGE-EXPECTED-DEFAULT-PROGRAMME",
            "Operator policy",
            "--expected-default-programme",
            RuleClassification::Policy,
            "/audioFormatExtended/audioProgramme",
            "the deterministic default programme matches the operator-declared expectation",
            format!(
                "expected={expected}, exists={exists}, selected={}",
                default.as_deref().unwrap_or("not resolved")
            ),
            true,
            matches,
        );
    }
    default
}

fn parse_programme_id(id: &str) -> Option<u16> {
    let value = id.strip_prefix("APR_")?;
    if value.len() != 4 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(value, 16).ok()
}

fn audit_importance(
    parsed: &ParsedXml,
    objects: &BTreeMap<String, usize>,
    object_limit: Option<usize>,
    rules: &mut Vec<Rule>,
) -> ImportanceReport {
    let mut entries = Vec::new();
    let mut object_importance = Vec::with_capacity(objects.len());
    let mut object_explicit = 0;
    let mut object_unspecified = 0;
    let mut pack_explicit = 0;
    let mut pack_unspecified = 0;
    let mut block_explicit = 0;
    let mut block_unspecified = 0;
    let mut invalid_values = 0;

    for node in &parsed.nodes {
        let canonical = canonical_element_name(&node.name);
        let Some((element, id_attribute, name_attribute, semantic_effect, recommended_use)) =
            (match canonical {
                "audioObject" => Some((
                    "audioObject",
                    "audioObjectID",
                    "audioObjectName",
                    "content",
                    "discard or combine only below a selected object threshold",
                )),
                "audioPackFormat" => Some((
                    "audioPackFormat",
                    "audioPackFormatID",
                    "audioPackFormatName",
                    "format-quality",
                    "use nested packs to reduce spatial quality without treating content as absent",
                )),
                "audioBlockFormat" => Some((
                    "audioBlockFormat",
                    "audioBlockFormatID",
                    "audioBlockFormatName",
                    "informational",
                    "do not drive discard; audioPackFormat importance takes precedence",
                )),
                _ => None,
            })
        else {
            continue;
        };
        let raw = node.attributes.get("importance");
        match (canonical, raw) {
            ("audioObject", Some(_)) => object_explicit += 1,
            ("audioObject", None) => object_unspecified += 1,
            ("audioPackFormat", Some(_)) => pack_explicit += 1,
            ("audioPackFormat", None) => pack_unspecified += 1,
            ("audioBlockFormat", Some(_)) => block_explicit += 1,
            ("audioBlockFormat", None) => block_unspecified += 1,
            _ => unreachable!("filtered importance element"),
        }
        let value = raw.and_then(|raw| raw.trim().parse::<u8>().ok());
        let valid = raw.is_none() || value.is_some_and(|value| value <= 10);
        if raw.is_some() && !valid {
            invalid_values += 1;
        }
        if let Some(raw) = raw {
            let subject = node
                .attributes
                .get(id_attribute)
                .cloned()
                .unwrap_or_else(|| element.to_owned());
            push_rule(
                rules,
                "BS2076-3-IMPORTANCE-RANGE",
                ADM_STANDARD,
                "§ 9.2",
                RuleClassification::Normative,
                &subject,
                "explicit importance is an integer from 0 (least) through 10 (most)",
                raw,
                true,
                valid,
            );
            entries.push(ImportanceEntry {
                element,
                element_id: node.attributes.get(id_attribute).cloned(),
                element_name: node.attributes.get(name_attribute).cloned(),
                raw_value: raw.clone(),
                value: value.filter(|value| *value <= 10),
                semantic_effect,
                recommended_use,
                valid,
            });
        }
        if canonical == "audioObject" {
            let id = node
                .attributes
                .get("audioObjectID")
                .cloned()
                .unwrap_or_else(|| "audioObject-without-ID".into());
            object_importance.push(ObjectImportance {
                id,
                value: value.filter(|value| *value <= 10),
                valid,
            });
        }
    }
    entries.sort_by(|left, right| {
        (left.element, &left.element_id).cmp(&(right.element, &right.element_id))
    });
    object_importance.sort_by(|left, right| left.id.cmp(&right.id));

    push_rule(
        rules,
        "BS2388-7-BLOCK-IMPORTANCE-INFORMATIONAL",
        USAGE_GUIDELINE,
        "§ 3.12.1",
        RuleClassification::Guidance,
        "/audioFormatExtended/audioChannelFormat/audioBlockFormat",
        "treat audioBlockFormat importance as fixed informational metadata and prefer audioPackFormat for quality compromise",
        format!("{block_explicit} explicit audioBlockFormat importance value(s)"),
        false,
        true,
    );

    let object_threshold_plan =
        object_limit.map(|limit| build_object_threshold_plan(&object_importance, limit, rules));
    ImportanceReport {
        audio_object_explicit: object_explicit,
        audio_object_unspecified: object_unspecified,
        audio_pack_format_explicit: pack_explicit,
        audio_pack_format_unspecified: pack_unspecified,
        audio_block_format_explicit: block_explicit,
        audio_block_format_unspecified: block_unspecified,
        invalid_values,
        audio_block_format_values_are_informational: true,
        entries,
        object_threshold_plan,
    }
}

fn build_object_threshold_plan(
    objects: &[ObjectImportance],
    target: usize,
    rules: &mut Vec<Rule>,
) -> ObjectThresholdPlan {
    let mut steps = Vec::with_capacity(11);
    let mut selected_threshold = None;
    let mut resulting_object_count = objects.len();
    for threshold in 0_u8..=10 {
        let retained = objects
            .iter()
            .filter(|object| {
                !object.valid
                    || object.value.is_none()
                    || object.value == Some(10)
                    || object.value.is_some_and(|value| value >= threshold)
            })
            .count();
        let discarded = objects.len() - retained;
        steps.push(ThresholdStep {
            threshold,
            retained_object_count: retained,
            discard_candidate_count: discarded,
        });
        resulting_object_count = retained;
        if selected_threshold.is_none() && retained <= target {
            selected_threshold = Some(threshold);
            break;
        }
    }
    let achievable = selected_threshold.is_some();
    let discard_candidates = selected_threshold
        .map(|threshold| {
            objects
                .iter()
                .filter(|object| {
                    object.valid
                        && object
                            .value
                            .is_some_and(|value| value < threshold && value < 10)
                })
                .map(|object| object.id.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let protected_importance_10 = objects
        .iter()
        .filter(|object| object.value == Some(10))
        .count();
    let protected_unspecified_or_invalid = objects
        .iter()
        .filter(|object| !object.valid || object.value.is_none())
        .count();
    push_rule(
        rules,
        "BS2388-7-OBJECT-IMPORTANCE-THRESHOLD",
        USAGE_GUIDELINE,
        "§§ 3.12.3-3.12.4",
        RuleClassification::Policy,
        "/audioFormatExtended/audioObject",
        "meet the requested object metadata count by increasing a 0..10 threshold without discarding importance-10 or unranked objects",
        format!(
            "target={target}, start={}, selected={}, result={resulting_object_count}, protected-10={protected_importance_10}, protected-unranked={protected_unspecified_or_invalid}",
            objects.len(),
            selected_threshold
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".into())
        ),
        true,
        achievable,
    );
    ObjectThresholdPlan {
        target_object_count: target,
        starting_object_count: objects.len(),
        achievable_without_protected_object_discard: achievable,
        selected_threshold,
        resulting_object_count,
        protected_importance_10,
        protected_unspecified_or_invalid,
        discard_candidates,
        steps,
        requires_renderer_or_merge: !achievable,
        note: IMPORTANCE_PLAN_NOTE,
    }
}

fn audit_tag_groups(
    parsed: &ParsedXml,
    programmes: &BTreeMap<String, usize>,
    contents: &BTreeMap<String, usize>,
    objects: &BTreeMap<String, usize>,
    rules: &mut Vec<Rule>,
) -> Vec<TagGroupAudit> {
    let programme_ids = canonical_ids(programmes);
    let content_ids = canonical_ids(contents);
    let object_ids = canonical_ids(objects);
    let tag_lists = parsed
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| node.name == "tagList")
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    push_rule(
        rules,
        "BS2076-3-TAG-LIST-CARDINALITY",
        ADM_STANDARD,
        "§ 5.12.1, Table A1-60",
        RuleClassification::Normative,
        "/audioFormatExtended/tagList",
        "audioFormatExtended contains zero or one tagList",
        format!("{} tagList element(s)", tag_lists.len()),
        true,
        tag_lists.len() <= 1,
    );
    let group_indices = tag_lists
        .iter()
        .flat_map(|tag_list| direct_children(parsed, *tag_list, "tagGroup"))
        .collect::<Vec<_>>();
    let group_cardinality_passed = tag_lists.is_empty() || !group_indices.is_empty();
    push_rule(
        rules,
        "BS2076-3-TAG-GROUP-CARDINALITY",
        ADM_STANDARD,
        "§ 5.11, Table A1-58",
        RuleClassification::Normative,
        "/audioFormatExtended/tagList",
        "a present tagList contains one or more tagGroup elements",
        format!("{} direct tagGroup element(s)", group_indices.len()),
        true,
        group_cardinality_passed,
    );

    let mut audits = Vec::new();
    for (ordinal, index) in group_indices.into_iter().enumerate() {
        let tags = direct_children(parsed, index, "tag")
            .into_iter()
            .map(|tag| TagValue {
                class: parsed.nodes[tag].attributes.get("class").cloned(),
                value: parsed.nodes[tag].text.trim().to_owned(),
            })
            .collect::<Vec<_>>();
        let mut references = Vec::new();
        for (name, element, definitions) in [
            ("audioProgrammeIDRef", "audioProgramme", &programme_ids),
            ("audioContentIDRef", "audioContent", &content_ids),
            ("audioObjectIDRef", "audioObject", &object_ids),
        ] {
            references.extend(
                direct_children(parsed, index, name)
                    .into_iter()
                    .map(|reference| TagReference {
                        element,
                        id: parsed.nodes[reference].text.trim().to_owned(),
                        resolves_locally: definitions
                            .contains(&parsed.nodes[reference].text.trim().to_ascii_uppercase()),
                    }),
            );
        }
        let association_passed = !tags.is_empty() && !references.is_empty();
        push_rule(
            rules,
            "BS2076-3-TAG-GROUP-CONTENT",
            ADM_STANDARD,
            "§ 5.11, Tables A1-58 and A1-59",
            RuleClassification::Normative,
            format!("tagGroup[{}]", ordinal + 1),
            "tagGroup contains at least one tag and at least one programme, content, or object reference",
            format!("tags={}, references={}", tags.len(), references.len()),
            true,
            association_passed,
        );
        let references_passed = references
            .iter()
            .all(|reference| reference.resolves_locally);
        push_rule(
            rules,
            "BS2076-3-TAG-LOCAL-REFERENCES",
            ADM_STANDARD,
            "§ 5.11, Table A1-59",
            RuleClassification::Normative,
            format!("tagGroup[{}]", ordinal + 1),
            "tagGroup programme, content, and object references resolve to local ADM elements",
            format!(
                "{} of {} reference(s) resolve locally",
                references
                    .iter()
                    .filter(|reference| reference.resolves_locally)
                    .count(),
                references.len()
            ),
            true,
            references_passed,
        );
        audits.push(TagGroupAudit {
            index: ordinal + 1,
            tags,
            references,
            semantic_authority: false,
            passed: association_passed && references_passed,
        });
    }
    audits
}

#[allow(clippy::too_many_arguments)]
fn push_rule(
    rules: &mut Vec<Rule>,
    rule_id: &'static str,
    authority: &'static str,
    section: &'static str,
    classification: RuleClassification,
    subject: impl Into<String>,
    requirement: impl Into<String>,
    observed: impl Into<String>,
    enforced: bool,
    passed: bool,
) {
    rules.push(Rule {
        rule_id,
        authority,
        section,
        classification,
        subject: subject.into(),
        requirement: requirement.into(),
        observed: observed.into(),
        enforced,
        passed,
    });
}

fn report_item_count(
    dialogue: &[DialogueAudit],
    avs: &[AvsReferenceAudit],
    importance: &ImportanceReport,
    tags: &[TagGroupAudit],
    rules: &[Rule],
) -> Result<usize, String> {
    let nested_avs = avs
        .iter()
        .map(|audit| audit.references.len())
        .sum::<usize>();
    let nested_tags = tags
        .iter()
        .map(|group| group.tags.len() + group.references.len())
        .sum::<usize>();
    let threshold_steps = importance
        .object_threshold_plan
        .as_ref()
        .map_or(0, |plan| plan.steps.len() + plan.discard_candidates.len());
    [
        dialogue.len(),
        avs.len(),
        nested_avs,
        importance.entries.len(),
        threshold_steps,
        tags.len(),
        nested_tags,
        rules.len(),
    ]
    .into_iter()
    .try_fold(0_usize, |total, count| {
        total
            .checked_add(count)
            .ok_or_else(|| "ADM semantics report item count overflow".to_string())
    })
}

fn validate_options(options: &Options) -> Result<(), String> {
    validate_limit("programme", options.max_programmes, HARD_MAX_PROGRAMMES)?;
    validate_limit("content", options.max_contents, HARD_MAX_CONTENTS)?;
    validate_limit("object", options.max_objects, HARD_MAX_OBJECTS)?;
    validate_limit(
        "report item",
        options.max_report_items,
        HARD_MAX_REPORT_ITEMS,
    )?;
    validate_limit("axml byte", options.max_axml_bytes, HARD_MAX_AXML_BYTES)?;
    validate_limit("XML node", options.max_xml_nodes, HARD_MAX_XML_NODES)?;
    if options
        .renderer_object_limit
        .is_some_and(|limit| limit == 0 || limit > HARD_MAX_OBJECTS)
    {
        return Err(format!(
            "renderer object limit must be 1..={HARD_MAX_OBJECTS}"
        ));
    }
    if options
        .expected_default_programme
        .as_deref()
        .is_some_and(str::is_empty)
    {
        return Err("expected default programme must not be empty".into());
    }
    Ok(())
}

fn validate_limit(label: &str, value: usize, hard_max: usize) -> Result<(), String> {
    if value == 0 || value > hard_max {
        Err(format!("{label} limit must be 1..={hard_max}"))
    } else {
        Ok(())
    }
}

fn enforce_count(label: &str, count: usize, limit: usize) -> Result<(), String> {
    if count > limit {
        Err(format!(
            "ADM contains {count} {label} elements, exceeding the configured limit {limit}"
        ))
    } else {
        Ok(())
    }
}

fn parse_xml(xml: &[u8], max_nodes: usize) -> Result<ParsedXml, String> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut parsed = ParsedXml::default();
    let mut stack = Vec::<usize>::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                let index = push_node(&element, stack.last().copied(), &mut parsed, max_nodes)?;
                stack.push(index);
            }
            Ok(Event::Empty(element)) => {
                push_node(&element, stack.last().copied(), &mut parsed, max_nodes)?;
            }
            Ok(Event::Text(text)) => {
                if let Some(index) = stack.last() {
                    let decoded = text.xml10_content();
                    parsed.nodes[*index].text.push_str(
                        &quick_xml::escape::unescape(&decoded)
                            .map_err(|error| format!("ADM XML entity: {error}"))?,
                    );
                }
            }
            Ok(Event::CData(text)) => {
                if let Some(index) = stack.last() {
                    parsed.nodes[*index].text.push_str(&text.xml10_content());
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                if let Some(index) = stack.last() {
                    let escaped = format!("&{};", reference.xml10_content());
                    parsed.nodes[*index].text.push_str(
                        &quick_xml::escape::unescape(&escaped)
                            .map_err(|error| format!("ADM XML entity: {error}"))?,
                    );
                }
            }
            Ok(Event::DocType(_)) => return Err("ADM XML document types are not accepted".into()),
            Ok(Event::End(_)) => {
                if stack.pop().is_none() {
                    return Err("ADM XML contains an unmatched closing element".into());
                }
            }
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(format!(
                    "ADM XML error at byte {}: {error}",
                    reader.error_position()
                ));
            }
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err("ADM XML ended with unclosed elements".into());
    }
    Ok(parsed)
}

fn push_node(
    element: &quick_xml::events::BytesStart<'_>,
    parent: Option<usize>,
    parsed: &mut ParsedXml,
    max_nodes: usize,
) -> Result<usize, String> {
    if parsed.nodes.len() >= max_nodes {
        return Err(format!(
            "ADM XML exceeds the configured {max_nodes} node limit"
        ));
    }
    let name = local_name(element.name().as_ref());
    let mut attributes = HashMap::new();
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| format!("ADM XML attribute: {error}"))?;
        let key = local_name(attribute.key.as_ref());
        let value = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|error| format!("ADM XML attribute {key}: {error}"))?
            .into_owned();
        if attributes.insert(key.clone(), value).is_some() {
            return Err(format!("ADM XML repeats attribute {key}"));
        }
    }
    let index = parsed.nodes.len();
    parsed.nodes.push(Node {
        name,
        parent,
        attributes,
        text: String::new(),
    });
    Ok(index)
}

fn definitions(
    parsed: &ParsedXml,
    element: &str,
    attribute: &str,
) -> Result<BTreeMap<String, usize>, String> {
    let mut values = BTreeMap::new();
    let mut canonical_values = HashSet::new();
    for (index, node) in parsed
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| node.name == element)
    {
        let id = node
            .attributes
            .get(attribute)
            .map(String::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("{element} is missing {attribute}"))?;
        if !canonical_values.insert(id.to_ascii_uppercase())
            || values.insert(id.to_owned(), index).is_some()
        {
            return Err(format!("{element} IDs must be unique: {id}"));
        }
    }
    Ok(values)
}

fn canonical_ids(definitions: &BTreeMap<String, usize>) -> HashSet<String> {
    definitions
        .keys()
        .map(|id| id.to_ascii_uppercase())
        .collect()
}

fn direct_children(parsed: &ParsedXml, parent: usize, name: &str) -> Vec<usize> {
    parsed
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| node.parent == Some(parent) && node.name == name)
        .map(|(index, _)| index)
        .collect()
}

fn count_nodes(parsed: &ParsedXml, name: &str) -> usize {
    parsed.nodes.iter().filter(|node| node.name == name).count()
}

fn count_canonical_nodes(parsed: &ParsedXml, name: &str) -> usize {
    parsed
        .nodes
        .iter()
        .filter(|node| canonical_element_name(&node.name) == name)
        .count()
}

fn canonical_element_name(name: &str) -> &str {
    if name.starts_with("audioBlockFormat") {
        "audioBlockFormat"
    } else {
        name
    }
}

fn local_name(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_owned()
}

fn ensure_regular_file(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("stat ADM input {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err("ADM input must be a regular file".into());
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<(String, u64), String> {
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut bytes = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| "file length overflow".to_string())?;
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    Ok((hex, bytes))
}

fn ensure_unchanged(path: &Path, expected_sha256: &str, expected_bytes: u64) -> Result<(), String> {
    let (sha256, bytes) = sha256_file(path)?;
    if sha256 != expected_sha256 || bytes != expected_bytes {
        return Err("ADM input changed while it was being audited".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_fixture(xml: &str) -> ParsedXml {
        parse_xml(xml.as_bytes(), DEFAULT_MAX_XML_NODES).unwrap()
    }

    #[test]
    fn validates_dialogue_value_attribute_and_kind_together() {
        let xml = r#"<audioFormatExtended><audioContent audioContentID="ACO_1001" audioContentName="AD"><dialogue dialogueContentKind="4">1</dialogue></audioContent></audioFormatExtended>"#;
        let parsed = parse_fixture(xml);
        let contents = definitions(&parsed, "audioContent", "audioContentID").unwrap();
        let mut rules = Vec::new();
        let audits = audit_dialogue(&parsed, &contents, &mut rules);
        assert_eq!(
            audits[0].content_kind,
            Some("audio-description-visually-impaired")
        );
        assert!(audits[0].passed);

        let xml = r#"<audioFormatExtended><audioContent audioContentID="ACO_1001"><dialogue mixedContentKind="6">1</dialogue></audioContent></audioFormatExtended>"#;
        let parsed = parse_fixture(xml);
        let contents = definitions(&parsed, "audioContent", "audioContentID").unwrap();
        let mut rules = Vec::new();
        let audits = audit_dialogue(&parsed, &contents, &mut rules);
        assert!(!audits[0].passed);
        assert!(rules
            .iter()
            .any(|rule| { rule.rule_id == "BS2076-3-DIALOGUE-KIND-ATTRIBUTE" && !rule.passed }));
    }

    #[test]
    fn rejects_two_avs_references_from_the_same_object() {
        let xml = r#"<audioFormatExtended>
          <audioContent audioContentID="ACO_1001"><alternativeValueSetIDRef>AVS_10af_0001</alternativeValueSetIDRef><alternativeValueSetIDRef>AVS_10AF_0002</alternativeValueSetIDRef></audioContent>
          <audioObject audioObjectID="AO_10Af"><alternativeValueSet alternativeValueSetID="AVS_10af_0001"/><alternativeValueSet alternativeValueSetID="AVS_10AF_0002"/></audioObject>
        </audioFormatExtended>"#;
        let parsed = parse_fixture(xml);
        let programmes = definitions(&parsed, "audioProgramme", "audioProgrammeID").unwrap();
        let contents = definitions(&parsed, "audioContent", "audioContentID").unwrap();
        let mut rules = Vec::new();
        let (audits, count) =
            audit_avs_references(&parsed, &programmes, &contents, &mut rules).unwrap();
        assert_eq!(count, 2);
        assert!(!audits[0].unique_object_groups);
        assert!(!audits[0].passed);
    }

    #[test]
    fn compares_hexadecimal_id_letters_case_insensitively() {
        let xml = r#"<audioFormatExtended>
          <audioContent audioContentID="ACO_1001"><alternativeValueSetIDRef>AVS_10AF_0001</alternativeValueSetIDRef></audioContent>
          <audioObject audioObjectID="AO_10af"><alternativeValueSet alternativeValueSetID="AVS_10af_0001"/></audioObject>
        </audioFormatExtended>"#;
        let parsed = parse_fixture(xml);
        let programmes = definitions(&parsed, "audioProgramme", "audioProgrammeID").unwrap();
        let contents = definitions(&parsed, "audioContent", "audioContentID").unwrap();
        let mut rules = Vec::new();
        let (audits, count) =
            audit_avs_references(&parsed, &programmes, &contents, &mut rules).unwrap();
        assert_eq!(count, 1);
        assert!(audits[0].references[0].resolves_locally);
        assert!(audits[0].references[0].owner_matches_id);
        assert!(audits[0].passed);

        let duplicate_xml = r#"<audioFormatExtended>
          <audioObject audioObjectID="AO_10af"/>
          <audioObject audioObjectID="AO_10AF"/>
        </audioFormatExtended>"#;
        let parsed = parse_fixture(duplicate_xml);
        let error = definitions(&parsed, "audioObject", "audioObjectID").unwrap_err();
        assert!(error.contains("IDs must be unique"));
    }

    #[test]
    fn selects_lowest_programme_id_numerically() {
        let xml = r#"<audioFormatExtended><audioProgramme audioProgrammeID="APR_0010"/><audioProgramme audioProgrammeID="APR_000A"/></audioFormatExtended>"#;
        let parsed = parse_fixture(xml);
        let programmes = definitions(&parsed, "audioProgramme", "audioProgrammeID").unwrap();
        let mut rules = Vec::new();
        let selected = audit_default_programme(&programmes, Some("APR_000A"), &mut rules);
        assert_eq!(selected.as_deref(), Some("APR_000A"));
        assert!(rules
            .iter()
            .filter(|rule| rule.enforced)
            .all(|rule| rule.passed));
    }

    #[test]
    fn threshold_plan_protects_ten_and_unspecified_objects() {
        let objects = vec![
            ObjectImportance {
                id: "AO_1001".into(),
                value: Some(10),
                valid: true,
            },
            ObjectImportance {
                id: "AO_1002".into(),
                value: Some(2),
                valid: true,
            },
            ObjectImportance {
                id: "AO_1003".into(),
                value: None,
                valid: true,
            },
        ];
        let mut rules = Vec::new();
        let plan = build_object_threshold_plan(&objects, 2, &mut rules);
        assert!(plan.achievable_without_protected_object_discard);
        assert_eq!(plan.selected_threshold, Some(3));
        assert_eq!(plan.discard_candidates, ["AO_1002"]);

        let plan = build_object_threshold_plan(&objects, 1, &mut rules);
        assert!(!plan.achievable_without_protected_object_discard);
        assert!(plan.requires_renderer_or_merge);
    }

    #[test]
    fn infers_presentation_patterns_without_asserting_auto_intent() {
        assert_eq!(
            inferred_presentation_mode(2, 0),
            "fixed-programme-alternatives"
        );
        assert_eq!(
            inferred_presentation_mode(1, 2),
            "interactive-complementary"
        );
        let mut rules = Vec::new();
        audit_presentation_intent(
            PresentationIntent::Auto,
            2,
            0,
            "fixed-programme-alternatives",
            &mut rules,
        );
        assert!(!rules[0].enforced);
        assert!(rules[0].passed);
    }

    #[test]
    fn tags_are_inventory_only_and_need_an_association() {
        let xml = r#"<audioFormatExtended><tagList><tagGroup><tag class="genre">news</tag></tagGroup></tagList></audioFormatExtended>"#;
        let parsed = parse_fixture(xml);
        let mut rules = Vec::new();
        let programmes = definitions(&parsed, "audioProgramme", "audioProgrammeID").unwrap();
        let contents = definitions(&parsed, "audioContent", "audioContentID").unwrap();
        let objects = definitions(&parsed, "audioObject", "audioObjectID").unwrap();
        let groups = audit_tag_groups(&parsed, &programmes, &contents, &objects, &mut rules);
        assert!(!groups[0].semantic_authority);
        assert!(!groups[0].passed);
    }
}
