//! Bounded metadata audit for ADM personalization ranges.
//!
//! This module validates the object-level interaction envelope declared by
//! `audioObjectInteraction` without claiming that the resulting audio has been
//! rendered or measured.  Audio loudness and true-peak compliance remains a
//! separate endpoint-rendering task.

use crate::metadata;
use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;
pub const REPORT_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/adm-interactivity-report-v1";
pub const VALIDATOR: &str = "forge-adm-interactivity-qc-1";
pub const ADM_STANDARD: &str = "ITU-R BS.2076-3";
pub const EMISSION_PROFILE_STANDARD: &str = "ITU-R BS.2168-0";
pub const DEFAULT_MAX_OBJECTS: usize = 4096;
pub const HARD_MAX_OBJECTS: usize = 65_535;
pub const DEFAULT_MAX_CONFIGURATIONS: usize = 4096;
pub const HARD_MAX_CONFIGURATIONS: usize = 16_384;
pub const DEFAULT_MAX_AXML_BYTES: usize = 16 * 1024 * 1024;
pub const HARD_MAX_AXML_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_MAX_XML_NODES: usize = 250_000;
pub const HARD_MAX_XML_NODES: usize = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    Safety,
    Bs2168EmissionRanges,
}

impl Profile {
    fn name(self) -> &'static str {
        match self {
            Self::Safety => "safety",
            Self::Bs2168EmissionRanges => "bs2168-emission-ranges",
        }
    }

    fn scope(self) -> &'static str {
        match self {
            Self::Safety => {
                "Object-level BS.2076-3 gain/position semantics plus Forge's explicit-upper-envelope safety policy"
            }
            Self::Bs2168EmissionRanges => {
                "Object-level BS.2168-0 audioObjectInteraction attribute and range subset; not full emission-profile validation"
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    pub input: PathBuf,
    pub profile: Profile,
    pub max_objects: usize,
    pub max_configurations: usize,
    pub max_axml_bytes: usize,
    pub max_xml_nodes: usize,
}

#[derive(Debug, Serialize)]
pub struct Limits {
    pub max_objects: usize,
    pub max_configurations: usize,
    pub max_axml_bytes: usize,
    pub max_xml_nodes: usize,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub schema_version: u32,
    pub validator: &'static str,
    pub adm_standard: &'static str,
    pub emission_profile_standard: Option<&'static str>,
    pub profile: &'static str,
    pub profile_scope: &'static str,
    pub input_path: String,
    pub input_bytes: u64,
    pub input_sha256: String,
    pub axml_bytes: usize,
    pub limits: Limits,
    pub object_count: usize,
    pub configuration_count: usize,
    pub interactive_configuration_count: usize,
    pub continuous_audio_compliance_verified: bool,
    pub endpoint_rendering_required: bool,
    pub scope_note: &'static str,
    pub passed: bool,
    pub configurations: Vec<ConfigurationAudit>,
}

#[derive(Debug, Serialize)]
pub struct ConfigurationAudit {
    pub object_id: String,
    pub object_name: String,
    pub alternative_value_set_id: Option<String>,
    pub interaction_inherited: bool,
    pub interact: bool,
    pub interaction_present: bool,
    pub on_off_interact_present: bool,
    pub on_off_interact: Option<bool>,
    pub gain_interact_present: bool,
    pub gain_interact: bool,
    pub position_interact_present: bool,
    pub position_interact: bool,
    pub default_gain: GainValue,
    pub gain_minimum: Option<GainValue>,
    pub gain_maximum: Option<GainValue>,
    pub position_ranges: Vec<PositionBoundary>,
    pub passed: bool,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GainValue {
    pub raw: String,
    pub unit: &'static str,
    pub linear: f64,
    pub db: Option<f64>,
    pub negative_infinity_db: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PositionBoundary {
    pub coordinate: String,
    pub bound: String,
    pub value: f64,
}

#[derive(Debug, Serialize)]
pub struct Rule {
    pub rule_id: &'static str,
    pub standard: &'static str,
    pub requirement: String,
    pub observed: String,
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
struct Interaction {
    present: bool,
    on_off_interact_present: bool,
    on_off: Option<bool>,
    gain_interact_present: bool,
    gain_interact: bool,
    position_interact_present: bool,
    position_interact: bool,
    gain_minimum: Option<GainValue>,
    gain_maximum: Option<GainValue>,
    gain_range_count: usize,
    position_ranges: Vec<PositionBoundary>,
}

impl Interaction {
    fn implicit_unbounded() -> Self {
        Self {
            present: false,
            on_off_interact_present: false,
            on_off: Some(true),
            gain_interact_present: false,
            gain_interact: true,
            position_interact_present: false,
            position_interact: true,
            gain_minimum: None,
            gain_maximum: None,
            gain_range_count: 0,
            position_ranges: Vec::new(),
        }
    }
}

pub fn run(options: &Options) -> Result<Report, String> {
    validate_options(options)?;
    let input = fs::canonicalize(&options.input)
        .map_err(|error| format!("resolve ADM input {}: {error}", options.input.display()))?;
    ensure_regular_file(&input)?;
    let (input_sha256, input_bytes) = sha256_file(&input)?;
    let axml = metadata::read_wave_chunk_limited(&input, *b"axml", options.max_axml_bytes)?
        .ok_or_else(|| "ADM interactivity QC requires an axml chunk".to_string())?;
    if metadata::read_wave_chunk_limited(&input, *b"chna", options.max_axml_bytes)?.is_none() {
        return Err("ADM interactivity QC requires a chna chunk".into());
    }
    let parsed = parse_xml(&axml, options.max_xml_nodes)?;
    let object_nodes = definitions(&parsed, "audioObject", "audioObjectID")?;
    if object_nodes.is_empty() {
        return Err("ADM contains no audioObject elements".into());
    }
    if object_nodes.len() > options.max_objects {
        return Err(format!(
            "ADM contains {} audioObject elements, exceeding the configured limit {}",
            object_nodes.len(),
            options.max_objects
        ));
    }

    let mut configurations = Vec::new();
    for (object_id, object_index) in object_nodes {
        let remaining_configurations = options.max_configurations - configurations.len();
        let object_configurations = audit_object(
            &parsed,
            object_index,
            &object_id,
            options.profile,
            remaining_configurations,
        )?;
        if configurations.len() + object_configurations.len() > options.max_configurations {
            return Err(format!(
                "ADM expands beyond the configured {} interaction-configuration limit",
                options.max_configurations
            ));
        }
        configurations.extend(object_configurations);
    }
    configurations.sort_by(|left, right| {
        (&left.object_id, &left.alternative_value_set_id)
            .cmp(&(&right.object_id, &right.alternative_value_set_id))
    });
    let interactive_configuration_count = configurations
        .iter()
        .filter(|configuration| {
            configuration.interact
                && (configuration.gain_interact
                    || configuration.position_interact
                    || configuration.on_off_interact == Some(true))
        })
        .count();
    let passed = configurations.iter().all(|item| item.passed);
    ensure_unchanged(&input, &input_sha256, input_bytes)?;

    Ok(Report {
        schema: REPORT_SCHEMA,
        schema_version: SCHEMA_VERSION,
        validator: VALIDATOR,
        adm_standard: ADM_STANDARD,
        emission_profile_standard: (options.profile == Profile::Bs2168EmissionRanges)
            .then_some(EMISSION_PROFILE_STANDARD),
        profile: options.profile.name(),
        profile_scope: options.profile.scope(),
        input_path: input.display().to_string(),
        input_bytes,
        input_sha256,
        axml_bytes: axml.len(),
        limits: Limits {
            max_objects: options.max_objects,
            max_configurations: options.max_configurations,
            max_axml_bytes: options.max_axml_bytes,
            max_xml_nodes: options.max_xml_nodes,
        },
        object_count: configurations
            .iter()
            .map(|item| item.object_id.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        configuration_count: configurations.len(),
        interactive_configuration_count,
        continuous_audio_compliance_verified: false,
        endpoint_rendering_required: interactive_configuration_count > 0,
        scope_note: "Object-level metadata range audit only; resolve nested gains, render, and independently measure bounded personalization cases before asserting loudness or true-peak compliance.",
        passed,
        configurations,
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
            "refusing to replace existing ADM interactivity report {}; pass --overwrite",
            path.display()
        ));
    }
    let mut bytes = if compact {
        serde_json::to_vec(report)
    } else {
        serde_json::to_vec_pretty(report)
    }
    .map_err(|error| format!("serialize ADM interactivity report: {error}"))?;
    bytes.push(b'\n');
    let mut output = crate::atomic::AtomicOutput::new_with_overwrite(path, overwrite)?;
    output.write_all(&bytes)?;
    output.commit()
}

fn audit_object(
    parsed: &ParsedXml,
    object: usize,
    object_id: &str,
    profile: Profile,
    max_configurations: usize,
) -> Result<Vec<ConfigurationAudit>, String> {
    let node = &parsed.nodes[object];
    let name = node
        .attributes
        .get("audioObjectName")
        .cloned()
        .unwrap_or_else(|| object_id.to_owned());
    let interact = optional_bool(node, "interact")?.unwrap_or(false);
    let base_gain = parse_single_gain(parsed, object)?.unwrap_or_else(unity_gain);
    let base_interaction_nodes = direct_children(parsed, object, "audioObjectInteraction");
    if base_interaction_nodes.len() > 1 {
        return Err(format!(
            "audioObject {object_id} contains multiple audioObjectInteraction elements"
        ));
    }
    let base_interaction = match base_interaction_nodes.first() {
        Some(index) => parse_interaction(parsed, *index)?,
        None if interact => Interaction::implicit_unbounded(),
        None => inactive_interaction(),
    };

    let mut results = vec![audit_configuration(
        object_id,
        &name,
        None,
        false,
        interact,
        base_gain.clone(),
        base_interaction.clone(),
        profile,
    )];
    let alternatives = direct_children(parsed, object, "alternativeValueSet");
    if alternatives.len() >= HARD_MAX_CONFIGURATIONS {
        return Err(format!(
            "audioObject {object_id} contains too many alternativeValueSet elements"
        ));
    }
    if alternatives.len() + 1 > max_configurations {
        return Err(format!(
            "ADM expands beyond the configured {max_configurations} interaction-configuration limit"
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for alternative in alternatives {
        let alternative_node = &parsed.nodes[alternative];
        let alternative_id = alternative_node
            .attributes
            .get("alternativeValueSetID")
            .ok_or_else(|| {
                format!("audioObject {object_id} contains an alternativeValueSet without an ID")
            })?
            .trim()
            .to_owned();
        if alternative_id.is_empty() || !seen.insert(alternative_id.clone()) {
            return Err(format!(
                "audioObject {object_id} alternativeValueSet IDs must be non-empty and unique: {alternative_id}"
            ));
        }
        let gain = parse_single_gain(parsed, alternative)?.unwrap_or_else(|| base_gain.clone());
        let interaction_nodes = direct_children(parsed, alternative, "audioObjectInteraction");
        if interaction_nodes.len() > 1 {
            return Err(format!(
                "alternativeValueSet {alternative_id} contains multiple audioObjectInteraction elements"
            ));
        }
        let (interaction, inherited) = match interaction_nodes.first() {
            Some(index) => (parse_interaction(parsed, *index)?, false),
            None => (base_interaction.clone(), true),
        };
        results.push(audit_configuration(
            object_id,
            &name,
            Some(alternative_id),
            inherited,
            interact,
            gain,
            interaction,
            profile,
        ));
    }
    Ok(results)
}

#[allow(clippy::too_many_arguments)]
fn audit_configuration(
    object_id: &str,
    object_name: &str,
    alternative_value_set_id: Option<String>,
    interaction_inherited: bool,
    interact: bool,
    default_gain: GainValue,
    interaction: Interaction,
    profile: Profile,
) -> ConfigurationAudit {
    let mut rules = Vec::new();
    push_rule(
        &mut rules,
        "BS2076-INTERACTION-CONTEXT",
        ADM_STANDARD,
        "audioObjectInteraction is absent unless audioObject interact is true",
        format!(
            "interact={interact}, interaction_present={}",
            interaction.present
        ),
        interact || !interaction.present,
    );
    push_rule(
        &mut rules,
        "BS2076-ONOFF-DECLARATION",
        ADM_STANDARD,
        "a present audioObjectInteraction declares onOffInteract",
        format!("onOffInteract={:?}", interaction.on_off),
        !interaction.present || interaction.on_off.is_some(),
    );

    let complete_gain_range = interaction.gain_range_count == 2
        && interaction.gain_minimum.is_some()
        && interaction.gain_maximum.is_some();
    push_rule(
        &mut rules,
        "FORGE-GAIN-RANGE-EXPLICIT",
        "Forge safety policy",
        "enabled gain interactivity has one explicit minimum and one explicit finite maximum",
        format!(
            "gainInteract={}, range_count={}, min={}, max={}",
            interaction.gain_interact,
            interaction.gain_range_count,
            describe_gain(interaction.gain_minimum.as_ref()),
            describe_gain(interaction.gain_maximum.as_ref())
        ),
        if interaction.gain_interact {
            complete_gain_range
                && interaction
                    .gain_maximum
                    .as_ref()
                    .is_some_and(|value| value.db.is_some())
        } else {
            true
        },
    );
    let ordered_gain = interaction
        .gain_minimum
        .as_ref()
        .zip(interaction.gain_maximum.as_ref())
        .is_none_or(|(minimum, maximum)| minimum.linear <= maximum.linear);
    push_rule(
        &mut rules,
        "BS2076-GAIN-BOUND-ORDER",
        ADM_STANDARD,
        "gain minimum does not exceed gain maximum",
        format!(
            "min={}, max={}",
            describe_gain(interaction.gain_minimum.as_ref()),
            describe_gain(interaction.gain_maximum.as_ref())
        ),
        ordered_gain,
    );
    let default_in_range = if interaction.gain_interact && complete_gain_range {
        interaction
            .gain_minimum
            .as_ref()
            .zip(interaction.gain_maximum.as_ref())
            .is_some_and(|(minimum, maximum)| {
                default_gain.linear >= minimum.linear && default_gain.linear <= maximum.linear
            })
    } else {
        true
    };
    push_rule(
        &mut rules,
        "FORGE-GAIN-DEFAULT-IN-RANGE",
        "Forge safety policy",
        "the default audioObject or alternativeValueSet gain lies inside the effective interaction range",
        format!("default={}", describe_gain(Some(&default_gain))),
        default_in_range,
    );

    let position_structure = position_structure(&interaction.position_ranges);
    push_rule(
        &mut rules,
        "FORGE-POSITION-RANGE-COMPLETE",
        "Forge safety policy",
        "enabled position interactivity has complete min/max coordinate pairs",
        format!(
            "positionInteract={}, ranges={}",
            interaction.position_interact,
            interaction.position_ranges.len()
        ),
        if interaction.position_interact {
            position_structure.complete && !interaction.position_ranges.is_empty()
        } else {
            true
        },
    );
    push_rule(
        &mut rules,
        "BS2076-POSITION-BOUND-ORDER",
        ADM_STANDARD,
        "every position minimum is finite and does not exceed its maximum",
        position_structure.observed.clone(),
        position_structure.ordered,
    );
    push_rule(
        &mut rules,
        "BS2076-POSITION-COORDINATE-SYSTEM",
        ADM_STANDARD,
        "position ranges use only polar coordinates or only Cartesian coordinates",
        position_structure.coordinate_system,
        position_structure.known_coordinates && !position_structure.mixed_coordinates,
    );

    if profile == Profile::Bs2168EmissionRanges {
        audit_emission_profile(interact, &default_gain, &interaction, &mut rules);
    }
    let passed = rules.iter().all(|rule| rule.passed);
    ConfigurationAudit {
        object_id: object_id.to_owned(),
        object_name: object_name.to_owned(),
        alternative_value_set_id,
        interaction_inherited,
        interact,
        interaction_present: interaction.present,
        on_off_interact_present: interaction.on_off_interact_present,
        on_off_interact: interaction.on_off,
        gain_interact_present: interaction.gain_interact_present,
        gain_interact: interaction.gain_interact,
        position_interact_present: interaction.position_interact_present,
        position_interact: interaction.position_interact,
        default_gain,
        gain_minimum: interaction.gain_minimum,
        gain_maximum: interaction.gain_maximum,
        position_ranges: interaction.position_ranges,
        passed,
        rules,
    }
}

fn audit_emission_profile(
    interact: bool,
    default_gain: &GainValue,
    interaction: &Interaction,
    rules: &mut Vec<Rule>,
) {
    push_rule(
        rules,
        "BS2168-ONOFF-DISABLED",
        EMISSION_PROFILE_STANDARD,
        "onOffInteract is false in the advanced-sound-system emission profile",
        format!("onOffInteract={:?}", interaction.on_off),
        !interact || interaction.on_off == Some(false),
    );
    let gain_pair = interaction.gain_range_count == 2
        && interaction.gain_minimum.is_some()
        && interaction.gain_maximum.is_some();
    push_rule(
        rules,
        "BS2168-GAIN-RANGE-PRESENCE",
        EMISSION_PROFILE_STANDARD,
        "gainInteractionRange is present exactly twice if and only if the gainInteract attribute is present",
        format!(
            "gainInteract_present={}, gainInteract={}, range_count={}",
            interaction.gain_interact_present,
            interaction.gain_interact,
            interaction.gain_range_count
        ),
        interaction.gain_interact_present == gain_pair,
    );
    let emission_gain_bounds = if interaction.gain_interact_present && gain_pair {
        interaction
            .gain_minimum
            .as_ref()
            .zip(interaction.gain_maximum.as_ref())
            .is_some_and(|(minimum, maximum)| {
                let minimum_ok =
                    minimum.negative_infinity_db || minimum.db.is_some_and(|value| value <= 0.0);
                let maximum_ok = maximum
                    .db
                    .is_some_and(|value| (0.0..=21.0).contains(&value));
                minimum_ok && maximum_ok
            })
    } else {
        !interaction.gain_interact_present && interaction.gain_range_count == 0
    };
    push_rule(
        rules,
        "BS2168-GAIN-RANGE-VALUES",
        EMISSION_PROFILE_STANDARD,
        "gain minimum is no greater than 0 dB and gain maximum is between 0 dB and +21 dB",
        format!(
            "min={}, max={}",
            describe_gain(interaction.gain_minimum.as_ref()),
            describe_gain(interaction.gain_maximum.as_ref())
        ),
        emission_gain_bounds,
    );
    let default_in_emission_range = interaction
        .gain_minimum
        .as_ref()
        .zip(interaction.gain_maximum.as_ref())
        .is_none_or(|(minimum, maximum)| {
            default_gain.linear >= minimum.linear && default_gain.linear <= maximum.linear
        });
    push_rule(
        rules,
        "BS2168-GAIN-DEFAULT-IN-RANGE",
        EMISSION_PROFILE_STANDARD,
        "the audioObject or alternativeValueSet gain does not exceed declared interaction limits",
        format!("default={}", describe_gain(Some(default_gain))),
        default_in_emission_range,
    );

    let position = &interaction.position_ranges;
    let position_presence = if interaction.position_interact_present {
        position.len() == 2
    } else {
        position.is_empty()
    };
    push_rule(
        rules,
        "BS2168-POSITION-RANGE-PRESENCE",
        EMISSION_PROFILE_STANDARD,
        "positionInteractionRange is present exactly twice if and only if the positionInteract attribute is present",
        format!(
            "positionInteract_present={}, positionInteract={}, range_count={}",
            interaction.position_interact_present,
            interaction.position_interact,
            position.len()
        ),
        position_presence,
    );
    let profile_position_values = if !interaction.position_interact_present {
        position.is_empty()
    } else if position.len() == 2 {
        let minimum = position.iter().find(|range| range.bound == "min");
        let maximum = position.iter().find(|range| range.bound == "max");
        minimum
            .zip(maximum)
            .is_some_and(|(minimum, maximum)| match minimum.coordinate.as_str() {
                "azimuth" if maximum.coordinate == "azimuth" => {
                    (-30.0..=0.0).contains(&minimum.value) && (0.0..=30.0).contains(&maximum.value)
                }
                "X" if maximum.coordinate == "X" => {
                    (-1.0..=0.0).contains(&minimum.value) && (0.0..=1.0).contains(&maximum.value)
                }
                _ => false,
            })
    } else {
        false
    };
    push_rule(
        rules,
        "BS2168-POSITION-RANGE-VALUES",
        EMISSION_PROFILE_STANDARD,
        "the emission profile uses one azimuth pair within -30..+30 degrees or one X pair within -1..+1",
        describe_positions(position),
        profile_position_values,
    );
}

fn parse_interaction(parsed: &ParsedXml, index: usize) -> Result<Interaction, String> {
    let node = &parsed.nodes[index];
    let on_off_interact_present = node.attributes.contains_key("onOffInteract");
    let on_off = optional_bool(node, "onOffInteract")?;
    let gain_interact_present = node.attributes.contains_key("gainInteract");
    let gain_interact = optional_bool(node, "gainInteract")?.unwrap_or(false);
    let position_interact_present = node.attributes.contains_key("positionInteract");
    let position_interact = optional_bool(node, "positionInteract")?.unwrap_or(false);
    let gain_nodes = direct_children(parsed, index, "gainInteractionRange");
    let mut gain_minimum = None;
    let mut gain_maximum = None;
    for gain_node in &gain_nodes {
        let bound = required_attribute(&parsed.nodes[*gain_node], "bound")?;
        let value = parse_gain_node(&parsed.nodes[*gain_node])?;
        match bound {
            "min" if gain_minimum.is_none() => gain_minimum = Some(value),
            "max" if gain_maximum.is_none() => gain_maximum = Some(value),
            "min" | "max" => return Err("gainInteractionRange repeats a bound".into()),
            _ => return Err(format!("unsupported gainInteractionRange bound {bound}")),
        }
    }
    let position_nodes = direct_children(parsed, index, "positionInteractionRange");
    if position_nodes.len() > 6 {
        return Err(
            "audioObjectInteraction contains more than six positionInteractionRange elements"
                .into(),
        );
    }
    let mut position_ranges = Vec::new();
    for position_node in position_nodes {
        let node = &parsed.nodes[position_node];
        let coordinate = required_attribute(node, "coordinate")?.to_owned();
        let bound = required_attribute(node, "bound")?.to_owned();
        if !matches!(bound.as_str(), "min" | "max") {
            return Err(format!(
                "unsupported positionInteractionRange bound {bound}"
            ));
        }
        let value = parse_finite(&node.text, "positionInteractionRange")?;
        position_ranges.push(PositionBoundary {
            coordinate,
            bound,
            value,
        });
    }
    Ok(Interaction {
        present: true,
        on_off_interact_present,
        on_off,
        gain_interact_present,
        gain_interact,
        position_interact_present,
        position_interact,
        gain_minimum,
        gain_maximum,
        gain_range_count: gain_nodes.len(),
        position_ranges,
    })
}

fn inactive_interaction() -> Interaction {
    Interaction {
        present: false,
        on_off_interact_present: false,
        on_off: None,
        gain_interact_present: false,
        gain_interact: false,
        position_interact_present: false,
        position_interact: false,
        gain_minimum: None,
        gain_maximum: None,
        gain_range_count: 0,
        position_ranges: Vec::new(),
    }
}

fn parse_single_gain(parsed: &ParsedXml, parent: usize) -> Result<Option<GainValue>, String> {
    let gains = direct_children(parsed, parent, "gain");
    if gains.len() > 1 {
        return Err(format!(
            "{} contains multiple gain elements",
            parsed.nodes[parent].name
        ));
    }
    gains
        .first()
        .map(|index| parse_gain_node(&parsed.nodes[*index]))
        .transpose()
}

fn parse_gain_node(node: &Node) -> Result<GainValue, String> {
    let raw = node.text.trim();
    if raw.is_empty() {
        return Err(format!("{} contains an empty gain value", node.name));
    }
    let unit = node
        .attributes
        .get("gainUnit")
        .map(String::as_str)
        .unwrap_or("linear");
    match unit {
        "linear" => {
            let linear = parse_finite(raw, &node.name)?;
            if linear < 0.0 {
                return Err(format!("{} linear gain must not be negative", node.name));
            }
            let negative_infinity_db = linear == 0.0;
            Ok(GainValue {
                raw: raw.to_owned(),
                unit: "linear",
                linear,
                db: (!negative_infinity_db).then(|| 20.0 * linear.log10()),
                negative_infinity_db,
            })
        }
        "dB" => {
            if matches!(raw, "-inf" | "-Inf" | "-INF") {
                return Ok(GainValue {
                    raw: raw.to_owned(),
                    unit: "dB",
                    linear: 0.0,
                    db: None,
                    negative_infinity_db: true,
                });
            }
            let db = parse_finite(raw, &node.name)?;
            let linear = 10_f64.powf(db / 20.0);
            if !linear.is_finite() {
                return Err(format!("{} dB gain is outside the finite range", node.name));
            }
            Ok(GainValue {
                raw: raw.to_owned(),
                unit: "dB",
                linear,
                db: Some(db),
                negative_infinity_db: false,
            })
        }
        _ => Err(format!("unsupported {} gainUnit {unit}", node.name)),
    }
}

fn unity_gain() -> GainValue {
    GainValue {
        raw: "1.0".into(),
        unit: "linear",
        linear: 1.0,
        db: Some(0.0),
        negative_infinity_db: false,
    }
}

struct PositionStructure {
    complete: bool,
    ordered: bool,
    known_coordinates: bool,
    mixed_coordinates: bool,
    coordinate_system: String,
    observed: String,
}

fn position_structure(ranges: &[PositionBoundary]) -> PositionStructure {
    let mut grouped = BTreeMap::<&str, (Option<f64>, Option<f64>, usize)>::new();
    for range in ranges {
        let entry = grouped.entry(&range.coordinate).or_default();
        entry.2 += 1;
        if range.bound == "min" {
            entry.0 = Some(range.value);
        } else if range.bound == "max" {
            entry.1 = Some(range.value);
        }
    }
    let complete = grouped
        .values()
        .all(|(minimum, maximum, count)| minimum.is_some() && maximum.is_some() && *count == 2);
    let ordered = grouped
        .values()
        .all(|(minimum, maximum, _)| minimum.zip(*maximum).is_none_or(|(min, max)| min <= max));
    let polar = grouped
        .keys()
        .any(|coordinate| matches!(*coordinate, "azimuth" | "elevation" | "distance"));
    let cartesian = grouped
        .keys()
        .any(|coordinate| matches!(*coordinate, "X" | "Y" | "Z"));
    let known_coordinates = grouped.keys().all(|coordinate| {
        matches!(
            *coordinate,
            "azimuth" | "elevation" | "distance" | "X" | "Y" | "Z"
        )
    });
    PositionStructure {
        complete,
        ordered,
        known_coordinates,
        mixed_coordinates: polar && cartesian,
        coordinate_system: if ranges.is_empty() {
            "none".into()
        } else if polar && cartesian {
            "mixed".into()
        } else if polar {
            "polar".into()
        } else if cartesian {
            "cartesian".into()
        } else {
            "unknown".into()
        },
        observed: describe_positions(ranges),
    }
}

fn describe_gain(value: Option<&GainValue>) -> String {
    match value {
        None => "missing".into(),
        Some(value) if value.negative_infinity_db => "-inf dB".into(),
        Some(value) => format!("{:.6} dB", value.db.unwrap_or_default()),
    }
}

fn describe_positions(ranges: &[PositionBoundary]) -> String {
    if ranges.is_empty() {
        return "none".into();
    }
    ranges
        .iter()
        .map(|range| format!("{}:{}={}", range.coordinate, range.bound, range.value))
        .collect::<Vec<_>>()
        .join(", ")
}

fn push_rule(
    rules: &mut Vec<Rule>,
    rule_id: &'static str,
    standard: &'static str,
    requirement: impl Into<String>,
    observed: impl Into<String>,
    passed: bool,
) {
    rules.push(Rule {
        rule_id,
        standard,
        requirement: requirement.into(),
        observed: observed.into(),
        passed,
    });
}

fn validate_options(options: &Options) -> Result<(), String> {
    if options.max_objects == 0 || options.max_objects > HARD_MAX_OBJECTS {
        return Err(format!("object limit must be 1..={HARD_MAX_OBJECTS}"));
    }
    if options.max_configurations == 0 || options.max_configurations > HARD_MAX_CONFIGURATIONS {
        return Err(format!(
            "configuration limit must be 1..={HARD_MAX_CONFIGURATIONS}"
        ));
    }
    if options.max_axml_bytes == 0 || options.max_axml_bytes > HARD_MAX_AXML_BYTES {
        return Err(format!("axml byte limit must be 1..={HARD_MAX_AXML_BYTES}"));
    }
    if options.max_xml_nodes == 0 || options.max_xml_nodes > HARD_MAX_XML_NODES {
        return Err(format!("XML node limit must be 1..={HARD_MAX_XML_NODES}"));
    }
    Ok(())
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
                    parsed.nodes[*index].text.push_str(text.as_ref().trim());
                }
            }
            Ok(Event::CData(text)) => {
                if let Some(index) = stack.last() {
                    parsed.nodes[*index].text.push_str(text.as_ref().trim());
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
    for (index, node) in parsed
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| node.name == element)
    {
        let id = required_attribute(node, attribute)?.trim();
        if id.is_empty() || values.insert(id.to_owned(), index).is_some() {
            return Err(format!("{element} IDs must be non-empty and unique: {id}"));
        }
    }
    Ok(values)
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

fn required_attribute<'a>(node: &'a Node, name: &str) -> Result<&'a str, String> {
    node.attributes
        .get(name)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{} is missing {name}", node.name))
}

fn optional_bool(node: &Node, name: &str) -> Result<Option<bool>, String> {
    node.attributes
        .get(name)
        .map(|value| match value.trim() {
            "1" | "true" => Ok(true),
            "0" | "false" => Ok(false),
            _ => Err(format!("{} {name} must be an XML boolean", node.name)),
        })
        .transpose()
}

fn parse_finite(value: &str, label: &str) -> Result<f64, String> {
    let parsed = value
        .trim()
        .parse::<f64>()
        .map_err(|error| format!("parse {label} value {value:?}: {error}"))?;
    if !parsed.is_finite() {
        return Err(format!("{label} must be finite"));
    }
    Ok(parsed)
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

    fn audit(xml: &str, profile: Profile) -> Vec<ConfigurationAudit> {
        let parsed = parse_xml(xml.as_bytes(), DEFAULT_MAX_XML_NODES).unwrap();
        let objects = definitions(&parsed, "audioObject", "audioObjectID").unwrap();
        objects
            .into_iter()
            .flat_map(|(id, index)| {
                audit_object(&parsed, index, &id, profile, HARD_MAX_CONFIGURATIONS).unwrap()
            })
            .collect()
    }

    #[test]
    fn complete_gain_range_passes_safety_and_emission_profiles() {
        let xml = r#"<audioFormatExtended><audioObject audioObjectID="AO_1001" audioObjectName="Dialogue" interact="1"><gain gainUnit="dB">0</gain><audioObjectInteraction onOffInteract="0" gainInteract="1"><gainInteractionRange bound="min" gainUnit="dB">-12</gainInteractionRange><gainInteractionRange bound="max" gainUnit="dB">6</gainInteractionRange></audioObjectInteraction></audioObject></audioFormatExtended>"#;
        for profile in [Profile::Safety, Profile::Bs2168EmissionRanges] {
            let result = audit(xml, profile);
            assert_eq!(result.len(), 1);
            assert!(result[0].passed, "{:#?}", result[0].rules);
        }
    }

    #[test]
    fn implicit_interaction_is_reported_as_unbounded() {
        let xml = r#"<audioFormatExtended><audioObject audioObjectID="AO_1001" interact="1"/></audioFormatExtended>"#;
        let result = audit(xml, Profile::Safety);
        assert!(!result[0].passed);
        assert!(result[0]
            .rules
            .iter()
            .any(|rule| rule.rule_id == "FORGE-GAIN-RANGE-EXPLICIT" && !rule.passed));
        assert_eq!(result[0].on_off_interact, Some(true));
        assert!(result[0].position_interact);
    }

    #[test]
    fn alternative_value_set_inherits_or_overrides_interaction() {
        let xml = r#"<audioFormatExtended><audioObject audioObjectID="AO_1001" interact="1"><audioObjectInteraction onOffInteract="0" gainInteract="1"><gainInteractionRange bound="min" gainUnit="dB">-12</gainInteractionRange><gainInteractionRange bound="max" gainUnit="dB">6</gainInteractionRange></audioObjectInteraction><alternativeValueSet alternativeValueSetID="AVS_1001_0001"><gain gainUnit="dB">3</gain></alternativeValueSet><alternativeValueSet alternativeValueSetID="AVS_1001_0002"><audioObjectInteraction onOffInteract="0" gainInteract="1"><gainInteractionRange bound="min" gainUnit="dB">-3</gainInteractionRange><gainInteractionRange bound="max" gainUnit="dB">3</gainInteractionRange></audioObjectInteraction></alternativeValueSet></audioObject></audioFormatExtended>"#;
        let result = audit(xml, Profile::Safety);
        assert_eq!(result.len(), 3);
        assert!(result[1].interaction_inherited);
        assert!(!result[2].interaction_inherited);
        assert!(result.iter().all(|item| item.passed));
    }

    #[test]
    fn emission_profile_rejects_switch_off_and_excess_gain() {
        let xml = r#"<audioFormatExtended><audioObject audioObjectID="AO_1001" interact="1"><audioObjectInteraction onOffInteract="1" gainInteract="1"><gainInteractionRange bound="min" gainUnit="dB">-12</gainInteractionRange><gainInteractionRange bound="max" gainUnit="dB">30</gainInteractionRange></audioObjectInteraction></audioObject></audioFormatExtended>"#;
        let result = audit(xml, Profile::Bs2168EmissionRanges);
        assert!(!result[0].passed);
        assert!(result[0]
            .rules
            .iter()
            .any(|rule| rule.rule_id == "BS2168-ONOFF-DISABLED" && !rule.passed));
        assert!(result[0]
            .rules
            .iter()
            .any(|rule| rule.rule_id == "BS2168-GAIN-RANGE-VALUES" && !rule.passed));
    }

    #[test]
    fn emission_profile_matches_ranges_to_attribute_presence() {
        let xml = r#"<audioFormatExtended><audioObject audioObjectID="AO_1001" interact="1"><audioObjectInteraction onOffInteract="0" gainInteract="0"><gainInteractionRange bound="min" gainUnit="dB">-12</gainInteractionRange><gainInteractionRange bound="max" gainUnit="dB">6</gainInteractionRange></audioObjectInteraction></audioObject></audioFormatExtended>"#;
        let result = audit(xml, Profile::Bs2168EmissionRanges);
        assert!(result[0].passed, "{:#?}", result[0].rules);
        assert!(result[0].gain_interact_present);
        assert!(!result[0].gain_interact);
    }

    #[test]
    fn rejects_mixed_or_incomplete_position_coordinates() {
        let xml = r#"<audioFormatExtended><audioObject audioObjectID="AO_1001" interact="1"><audioObjectInteraction onOffInteract="0" positionInteract="1"><positionInteractionRange coordinate="azimuth" bound="min">-30</positionInteractionRange><positionInteractionRange coordinate="X" bound="max">1</positionInteractionRange></audioObjectInteraction></audioObject></audioFormatExtended>"#;
        let result = audit(xml, Profile::Safety);
        assert!(!result[0].passed);
    }
}
