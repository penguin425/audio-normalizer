//! Adapter for the EBU ADM Toolbox reference implementation.
//!
//! Forge deliberately delegates the full ITU-R BS.2127 rendering algorithm to
//! `eat-process` instead of approximating object, HOA, matrix, or binaural
//! rendering. The adapter validates the input against the ITU-R BS.2168
//! emission profile, renders it to a BS.2051 layout, and then measures the
//! rendered loudspeaker signals with Forge's BS.1770 engine.

use crate::metadata;
use crate::normalize::{self, Analysis};
use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub const RENDERER_STANDARD: &str = "ITU-R BS.2127-1";
pub const PROFILE_STANDARD: &str = "ITU-R BS.2168-0";
pub const PRODUCTION_PROFILE_STANDARD: &str = "EBU Tech 3393";
pub const PRODUCTION_PROFILE_EDITION: &str = "September 2025";
pub const PRODUCTION_PROFILE_NAME: &str = "EBU Production Profile";
pub const PRODUCTION_PROFILE_VERSION: &str = "1.0";
pub const PRODUCTION_PROFILE_LEVEL: &str = "1";
pub const PRODUCTION_VALIDATOR: &str = "forge-tech3393-2025-bs2076-3-3";
pub const ADM_STANDARD: &str = "ITU-R BS.2076-3";
pub const ADM_VERSION: &str = "ITU-R_BS.2076-3";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductionProfileMode {
    Read,
    Write,
}

impl ProductionProfileMode {
    pub fn parse(value: &str) -> Self {
        if value == "write" {
            Self::Write
        } else {
            Self::Read
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AdmProfileRule {
    pub rule_id: &'static str,
    pub path: String,
    pub requirement: String,
    pub observed: String,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProductionProfileResult {
    pub path: String,
    pub standard: &'static str,
    pub adm_standard: &'static str,
    pub adm_version: &'static str,
    pub profile_name: &'static str,
    pub profile_version: &'static str,
    pub profile_level: &'static str,
    pub mode: ProductionProfileMode,
    pub validator: &'static str,
    pub passed: bool,
    pub rules: Vec<AdmProfileRule>,
}

#[derive(Debug, Default)]
struct ParsedAdm {
    roots: Vec<(String, Option<String>)>,
    profile_lists: usize,
    profiles: Vec<ParsedProfile>,
    tag_lists: usize,
    tag_groups: Vec<(String, usize)>,
    track_format_stream_refs: Vec<(String, usize)>,
    ids: Vec<ParsedId>,
    references: Vec<(String, String)>,
    time_values: Vec<(String, String)>,
    deprecated_mxf_lookups: Vec<String>,
    elements: Vec<ParsedElement>,
}

#[derive(Debug, Default)]
struct ParsedElement {
    name: String,
    path: String,
    parent: Option<usize>,
    attributes: HashMap<String, String>,
    children: HashMap<String, usize>,
    text: String,
}

#[derive(Debug)]
struct ParsedId {
    path: String,
    element: String,
    value: String,
}

#[derive(Debug, Default)]
struct ParsedProfile {
    path: String,
    text: String,
    attributes: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct ReferenceRendererOptions {
    pub command: PathBuf,
    pub layout: String,
    pub profile_level: u8,
    pub overwrite: bool,
}

impl Default for ReferenceRendererOptions {
    fn default() -> Self {
        Self {
            command: PathBuf::from("eat-process"),
            layout: "4+5+0".into(),
            profile_level: 0,
            overwrite: false,
        }
    }
}

#[derive(Debug)]
pub struct ReferenceRenderResult {
    pub analysis: Analysis,
    pub renderer: String,
    pub renderer_standard: &'static str,
    pub profile_standard: &'static str,
    pub profile_level: u8,
    pub layout: String,
    pub output_path: Option<PathBuf>,
}

pub fn validate_and_render(
    input: &Path,
    retained_output: Option<&Path>,
    options: &ReferenceRendererOptions,
) -> Result<ReferenceRenderResult, String> {
    validate_options(options)?;
    require_adm_chunks(input)?;
    if !options.overwrite && retained_output.is_some_and(Path::exists) {
        return Err(format!(
            "ADM rendered output already exists: {}",
            retained_output.unwrap().display()
        ));
    }

    let work = tempfile::Builder::new()
        .prefix("forge-adm-")
        .tempdir()
        .map_err(|error| format!("create ADM work directory: {error}"))?;
    let validate_config = work.path().join("validate.json");
    let render_config = work.path().join("render.json");
    let rendered = work.path().join("rendered.wav");
    write_config(&validate_config, &validation_config(options.profile_level))?;
    write_config(&render_config, &render_config_value())?;

    run_eat(
        options,
        &validate_config,
        &[("input.path", input.as_os_str())],
        "BS.2168 profile validation",
    )?;
    run_eat(
        options,
        &render_config,
        &[
            ("input.path", input.as_os_str()),
            (
                "render.layout",
                std::ffi::OsStr::new(options.layout.as_str()),
            ),
            ("output.path", rendered.as_os_str()),
        ],
        "BS.2127 rendering",
    )?;
    if !rendered.is_file() {
        return Err(format!(
            "ADM renderer succeeded without creating {}",
            rendered.display()
        ));
    }

    let analysis = normalize::analyze_file(&rendered)?;
    let output_path = retained_output
        .map(|destination| {
            fs::copy(&rendered, destination).map_err(|error| {
                format!(
                    "retain ADM render {} as {}: {error}",
                    rendered.display(),
                    destination.display()
                )
            })?;
            Ok::<_, String>(destination.to_path_buf())
        })
        .transpose()?;
    Ok(ReferenceRenderResult {
        analysis,
        renderer: options.command.display().to_string(),
        renderer_standard: RENDERER_STANDARD,
        profile_standard: PROFILE_STANDARD,
        profile_level: options.profile_level,
        layout: options.layout.clone(),
        output_path,
    })
}

/// Validate the machine-checkable EBU Tech 3393 profile declaration and core
/// interoperability constraints directly from the ADM `axml` chunk.
pub fn validate_production_profile(
    input: &Path,
    mode: ProductionProfileMode,
) -> Result<ProductionProfileResult, String> {
    let axml = metadata::read_wave_chunk(input, *b"axml")?
        .ok_or_else(|| "EBU Tech 3393 validation requires an axml chunk".to_string())?;
    let parsed = match parse_adm(&axml) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Ok(profile_result(
                input,
                mode,
                vec![AdmProfileRule {
                    rule_id: "TECH3393-XML",
                    path: "/".into(),
                    requirement: "axml shall contain well-formed XML".into(),
                    observed: error,
                    passed: false,
                }],
            ));
        }
    };
    let mut rules = vec![AdmProfileRule {
        rule_id: "TECH3393-XML",
        path: "/".into(),
        requirement: "axml shall contain well-formed XML".into(),
        observed: "well-formed".into(),
        passed: true,
    }];

    rules.push(AdmProfileRule {
        rule_id: "BS2076-3-AUDIO-FORMAT-EXTENDED",
        path: "/audioFormatExtended".into(),
        requirement: "exactly one audioFormatExtended root element".into(),
        observed: format!("{} root element(s)", parsed.roots.len()),
        passed: parsed.roots.len() == 1,
    });
    let version = parsed
        .roots
        .first()
        .and_then(|(_, version)| version.as_deref());
    rules.push(AdmProfileRule {
        rule_id: "TECH3393-2025-AFE-VERSION",
        path: "/audioFormatExtended/@version".into(),
        requirement: "Tech 3393:2025 Table 59: version shall be ITU-R_BS.2076-2 or ITU-R_BS.2076-3"
            .into(),
        observed: version.unwrap_or("not present").into(),
        passed: version.is_some_and(is_production_profile_adm_version),
    });
    rules.push(AdmProfileRule {
        rule_id: "BS2076-3-TAG-LIST-CARDINALITY",
        path: "/audioFormatExtended/tagList".into(),
        requirement: "zero or one tagList element".into(),
        observed: format!("{} tagList element(s)", parsed.tag_lists),
        passed: parsed.tag_lists <= 1,
    });
    for (path, reference_count) in &parsed.tag_groups {
        rules.push(AdmProfileRule {
            rule_id: "BS2076-3-TAG-GROUP-REFERENCE",
            path: path.clone(),
            requirement:
                "tagGroup shall reference at least one audioProgramme, audioContent, or audioObject"
                    .into(),
            observed: format!("{reference_count} associated ADM reference(s)"),
            passed: *reference_count > 0,
        });
    }
    rules.push(AdmProfileRule {
        rule_id: "BS2076-3-DEPRECATED-MXF-LOOKUP",
        path: "/audioFormatExtended/audioTrackUID/audioMXFLookUp".into(),
        requirement: "audioMXFLookUp shall not be used in BS.2076-3".into(),
        observed: if parsed.deprecated_mxf_lookups.is_empty() {
            "not present".into()
        } else {
            parsed.deprecated_mxf_lookups.join(", ")
        },
        passed: parsed.deprecated_mxf_lookups.is_empty(),
    });
    let invalid_times = parsed
        .time_values
        .iter()
        .filter(|(_, value)| !valid_adm_time(value))
        .map(|(path, value)| format!("{path}={value}"))
        .collect::<Vec<_>>();
    rules.push(AdmProfileRule {
        rule_id: "BS2076-3-TIME-FORMAT",
        path: "/audioFormatExtended".into(),
        requirement: "ADM time attributes shall use valid decimal or sample-fraction time syntax"
            .into(),
        observed: if invalid_times.is_empty() {
            format!("{} valid time value(s)", parsed.time_values.len())
        } else {
            format!("invalid value(s): {}", invalid_times.join(", "))
        },
        passed: invalid_times.is_empty(),
    });

    rules.extend(validate_tech3393_structure(&parsed, mode));

    let profile_list_pass = match mode {
        ProductionProfileMode::Read => parsed.profile_lists <= 1,
        ProductionProfileMode::Write => parsed.profile_lists == 1,
    };
    rules.push(AdmProfileRule {
        rule_id: "TECH3393-2025-PROFILE-LIST-CARDINALITY",
        path: "/audioFormatExtended/profileList".into(),
        requirement: match mode {
            ProductionProfileMode::Read => "zero or one profileList element".into(),
            ProductionProfileMode::Write => "exactly one profileList element".into(),
        },
        observed: format!("{} profileList element(s)", parsed.profile_lists),
        passed: profile_list_pass,
    });

    let count_pass = match mode {
        ProductionProfileMode::Read => parsed.profiles.len() <= 8,
        ProductionProfileMode::Write => (1..=8).contains(&parsed.profiles.len()),
    };
    rules.push(AdmProfileRule {
        rule_id: "TECH3393-2025-PROFILE-COUNT",
        path: "/audioFormatExtended/profileList/profile".into(),
        requirement: match mode {
            ProductionProfileMode::Read => {
                "Tech 3393:2025 Table 52: zero to eight profile elements".into()
            }
            ProductionProfileMode::Write => {
                "Tech 3393:2025 Table 52: one to eight profile elements".into()
            }
        },
        observed: format!("{} profile element(s)", parsed.profiles.len()),
        passed: count_pass,
    });

    let production = parsed
        .profiles
        .iter()
        .find(|profile| profile.text.trim() == PRODUCTION_PROFILE_STANDARD);
    rules.push(AdmProfileRule {
        rule_id: "TECH3393-2025-PROFILE-IDENTIFIER",
        path: "/audioFormatExtended/profileList/profile".into(),
        requirement: format!("one profile shall contain {PRODUCTION_PROFILE_STANDARD}"),
        observed: production
            .map(|profile| profile.text.trim().to_owned())
            .unwrap_or_else(|| "not present".into()),
        passed: match mode {
            ProductionProfileMode::Read => parsed.profiles.is_empty() || production.is_some(),
            ProductionProfileMode::Write => production.is_some(),
        },
    });

    if let Some(profile) = production {
        for (rule_id, attribute, expected) in [
            (
                "TECH3393-2025-PROFILE-NAME",
                "profileName",
                PRODUCTION_PROFILE_NAME,
            ),
            (
                "TECH3393-2025-PROFILE-VERSION",
                "profileVersion",
                PRODUCTION_PROFILE_VERSION,
            ),
            (
                "TECH3393-2025-PROFILE-LEVEL",
                "profileLevel",
                PRODUCTION_PROFILE_LEVEL,
            ),
        ] {
            let observed = profile.attributes.get(attribute);
            rules.push(AdmProfileRule {
                rule_id,
                path: format!("{}/@{attribute}", profile.path),
                requirement: match mode {
                    ProductionProfileMode::Read => {
                        format!("if present, {attribute} shall equal {expected}")
                    }
                    ProductionProfileMode::Write => {
                        format!("{attribute} shall be present and equal {expected}")
                    }
                },
                observed: observed.cloned().unwrap_or_else(|| "not present".into()),
                passed: match mode {
                    ProductionProfileMode::Read => observed.is_none_or(|value| value == expected),
                    ProductionProfileMode::Write => observed.is_some_and(|value| value == expected),
                },
            });
        }
    }

    let duplicate_ids = duplicate_ids(&parsed.ids);
    rules.push(AdmProfileRule {
        rule_id: "TECH3393-ADM-ID-UNIQUE",
        path: "/audioFormatExtended".into(),
        requirement: "ADM element IDs shall be unique".into(),
        observed: if duplicate_ids.is_empty() {
            format!("{} unique ID(s)", parsed.ids.len())
        } else {
            format!("duplicate ID(s): {}", duplicate_ids.join(", "))
        },
        passed: duplicate_ids.is_empty(),
    });

    let invalid_ids = parsed
        .ids
        .iter()
        .filter(|id| !valid_adm_id(&id.element, &id.value))
        .map(|id| format!("{}={}", id.path, id.value))
        .collect::<Vec<_>>();
    rules.push(AdmProfileRule {
        rule_id: "BS2076-3-ID-SYNTAX",
        path: "/audioFormatExtended".into(),
        requirement: "defined ADM IDs shall match the BS.2076-3 element-specific syntax".into(),
        observed: if invalid_ids.is_empty() {
            format!("{} valid ID(s)", parsed.ids.len())
        } else {
            format!("invalid ID(s): {}", invalid_ids.join(", "))
        },
        passed: invalid_ids.is_empty(),
    });

    let defined_ids = parsed
        .ids
        .iter()
        .map(|id| id.value.as_str())
        .collect::<HashSet<_>>();
    let unresolved = parsed
        .references
        .iter()
        .filter(|(_, reference)| {
            requires_local_definition(reference) && !defined_ids.contains(reference.as_str())
        })
        .map(|(path, reference)| format!("{path}={reference}"))
        .collect::<Vec<_>>();
    rules.push(AdmProfileRule {
        rule_id: "BS2076-3-LOCAL-REFERENCES",
        path: "/audioFormatExtended".into(),
        requirement:
            "programme, content, object, alternative-value-set, and track-UID references shall resolve locally"
                .into(),
        observed: if unresolved.is_empty() {
            format!("{} resolvable or common-definition reference(s)", parsed.references.len())
        } else {
            format!("unresolved reference(s): {}", unresolved.join(", "))
        },
        passed: unresolved.is_empty(),
    });

    if mode == ProductionProfileMode::Read {
        for (path, count) in &parsed.track_format_stream_refs {
            rules.push(AdmProfileRule {
                rule_id: "TECH3393-2025-TRACK-STREAM-REFERENCE",
                path: path.clone(),
                requirement: "Tech 3393:2025 Table 51: audioTrackFormat shall contain exactly one audioStreamFormatIDRef"
                    .into(),
                observed: format!("{count} audioStreamFormatIDRef element(s)"),
                passed: *count == 1,
            });
        }
    }

    rules.extend(validate_chna(input, &parsed)?);

    // Retain the observed reference count in an auditable rule without
    // rejecting common-definition references that are valid outside axml.
    rules.push(AdmProfileRule {
        rule_id: "TECH3393-ADM-REFERENCES-OBSERVED",
        path: "/audioFormatExtended".into(),
        requirement: "report ADM ID references for downstream validation".into(),
        observed: format!("{} ID reference(s)", parsed.references.len()),
        passed: true,
    });
    Ok(profile_result(input, mode, rules))
}

fn profile_result(
    input: &Path,
    mode: ProductionProfileMode,
    rules: Vec<AdmProfileRule>,
) -> ProductionProfileResult {
    ProductionProfileResult {
        path: input.to_string_lossy().into_owned(),
        standard: PRODUCTION_PROFILE_STANDARD,
        adm_standard: ADM_STANDARD,
        adm_version: ADM_VERSION,
        profile_name: PRODUCTION_PROFILE_NAME,
        profile_version: PRODUCTION_PROFILE_VERSION,
        profile_level: PRODUCTION_PROFILE_LEVEL,
        mode,
        validator: PRODUCTION_VALIDATOR,
        passed: rules.iter().all(|rule| rule.passed),
        rules,
    }
}

#[derive(Clone, Copy)]
struct ChildConstraint {
    name: &'static str,
    read_min: usize,
    read_max: usize,
    write_min: usize,
    write_max: usize,
}

fn validate_tech3393_structure(
    parsed: &ParsedAdm,
    mode: ProductionProfileMode,
) -> Vec<AdmProfileRule> {
    let mut rules = Vec::new();
    let roots = parsed
        .elements
        .iter()
        .filter(|element| element.name == "audioFormatExtended")
        .collect::<Vec<_>>();
    let root_count = |name: &str| {
        roots
            .iter()
            .map(|root| root.children.get(name).copied().unwrap_or(0))
            .sum::<usize>()
    };
    for (rule_id, element, minimum, read_maximum, write_maximum, table) in [
        (
            "TECH3393-2025-PROGRAMME-COUNT",
            "audioProgramme",
            1,
            32,
            32,
            "Tables 1 and 60",
        ),
        (
            "TECH3393-2025-CONTENT-COUNT",
            "audioContent",
            1,
            128,
            128,
            "Tables 1 and 60",
        ),
        (
            "TECH3393-2025-OBJECT-COUNT",
            "audioObject",
            1,
            128,
            128,
            "Tables 1 and 60",
        ),
        (
            "TECH3393-2025-TRACK-UID-COUNT",
            "audioTrackUID",
            1,
            128,
            128,
            "Tables 1 and 60",
        ),
        (
            "TECH3393-2025-PACK-FORMAT-COUNT",
            "audioPackFormat",
            0,
            256,
            256,
            "Tables 2 and 60",
        ),
        (
            "TECH3393-2025-CHANNEL-FORMAT-COUNT",
            "audioChannelFormat",
            0,
            1024,
            1024,
            "Tables 2 and 60",
        ),
        (
            "TECH3393-2025-STREAM-FORMAT-COUNT",
            "audioStreamFormat",
            0,
            1024,
            0,
            "Tables 2 and 60",
        ),
        (
            "TECH3393-2025-TRACK-FORMAT-COUNT",
            "audioTrackFormat",
            0,
            1024,
            0,
            "Tables 2 and 60",
        ),
        (
            "TECH3393-2025-TAG-LIST-COUNT",
            "tagList",
            if mode == ProductionProfileMode::Write {
                1
            } else {
                0
            },
            1,
            1,
            "Table 60",
        ),
    ] {
        let count = root_count(element);
        let maximum = if mode == ProductionProfileMode::Write {
            write_maximum
        } else {
            read_maximum
        };
        rules.push(AdmProfileRule {
            rule_id,
            path: format!("/audioFormatExtended/{element}"),
            requirement: format!(
                "Tech 3393:2025 {table}: {minimum} to {maximum} {element} element(s)"
            ),
            observed: format!("{count} element(s)"),
            passed: (minimum..=maximum).contains(&count),
        });
    }

    validate_required_attributes(
        parsed,
        mode,
        &mut rules,
        "audioProgramme",
        "TECH3393-2025-PROGRAMME-ATTRIBUTES",
        "Table 4",
        &["audioProgrammeID", "audioProgrammeName"],
        &["audioProgrammeID", "audioProgrammeName"],
    );
    validate_required_attributes(
        parsed,
        mode,
        &mut rules,
        "audioContent",
        "TECH3393-2025-CONTENT-ATTRIBUTES",
        "Table 15",
        &["audioContentID", "audioContentName"],
        &["audioContentID", "audioContentName"],
    );
    validate_required_attributes(
        parsed,
        mode,
        &mut rules,
        "audioObject",
        "TECH3393-2025-OBJECT-ATTRIBUTES",
        "Table 22",
        &["audioObjectID", "audioObjectName"],
        &["audioObjectID", "audioObjectName", "importance", "interact"],
    );
    validate_required_attributes(
        parsed,
        mode,
        &mut rules,
        "audioTrackUID",
        "TECH3393-2025-TRACK-UID-ATTRIBUTES",
        "Table 28",
        &["UID"],
        &["UID"],
    );
    validate_format_attributes(
        parsed,
        mode,
        &mut rules,
        "audioPackFormat",
        "audioPackFormatID",
        "audioPackFormatName",
        "TECH3393-2025-PACK-FORMAT-ATTRIBUTES",
        "Table 30",
    );
    validate_format_attributes(
        parsed,
        mode,
        &mut rules,
        "audioChannelFormat",
        "audioChannelFormatID",
        "audioChannelFormatName",
        "TECH3393-2025-CHANNEL-FORMAT-ATTRIBUTES",
        "Table 34",
    );
    if mode == ProductionProfileMode::Read {
        validate_format_attributes(
            parsed,
            mode,
            &mut rules,
            "audioStreamFormat",
            "audioStreamFormatID",
            "audioStreamFormatName",
            "TECH3393-2025-STREAM-FORMAT-ATTRIBUTES",
            "Table 48",
        );
        validate_format_attributes(
            parsed,
            mode,
            &mut rules,
            "audioTrackFormat",
            "audioTrackFormatID",
            "audioTrackFormatName",
            "TECH3393-2025-TRACK-FORMAT-ATTRIBUTES",
            "Table 50",
        );
    }

    validate_child_constraints(
        parsed,
        mode,
        &mut rules,
        "audioProgramme",
        "TECH3393-2025-PROGRAMME-CHILDREN",
        "Table 5",
        &[
            child("audioProgrammeLabel", 0, 16, 0, 16),
            child("audioContentIDRef", 1, 128, 1, 128),
            child("loudnessMetadata", 0, 1, 0, 1),
            child("audioProgrammeReferenceScreen", 0, 1, 0, 1),
            child("authoringInformation", 0, 1, 0, 1),
            child("alternativeValueSet", 0, 32, 0, 32),
        ],
    );
    validate_child_constraints(
        parsed,
        mode,
        &mut rules,
        "audioContent",
        "TECH3393-2025-CONTENT-CHILDREN",
        "Table 16",
        &[
            child("audioContentLabel", 0, 16, 0, 16),
            child("audioObjectIDRef", 1, 128, 1, 128),
            child("loudnessMetadata", 0, 1, 0, 1),
            child("dialogue", 0, 1, 1, 1),
            child("alternativeValueSet", 0, 4, 0, 4),
        ],
    );
    validate_child_constraints(
        parsed,
        mode,
        &mut rules,
        "audioObject",
        "TECH3393-2025-OBJECT-CHILDREN",
        "Table 23",
        &[
            child("audioPackFormatIDRef", 0, 1, 0, 1),
            child("audioObjectIDRef", 0, 128, 0, 128),
            child("audioObjectLabel", 0, 16, 0, 16),
            child("audioComplementaryObjectGroupLabel", 0, 16, 0, 16),
            child("audioComplementaryObjectIDRef", 0, 128, 0, 128),
            child("audioTrackUIDRef", 0, 128, 0, 128),
            child("audioObjectInteraction", 0, 1, 0, 1),
            child("gain", 0, 1, 0, 1),
            child("headLocked", 0, 1, 0, 1),
            child("positionOffset", 0, 1, 0, 1),
            child("mute", 0, 1, 0, 1),
            child("alternativeValueSet", 0, 16, 0, 16),
        ],
    );
    validate_track_uid_children(parsed, mode, &mut rules);
    validate_child_constraints(
        parsed,
        mode,
        &mut rules,
        "audioPackFormat",
        "TECH3393-2025-PACK-FORMAT-CHILDREN",
        "Tables 31 to 33",
        &[
            child("audioChannelFormatIDRef", 0, 1024, 0, 1024),
            child("audioPackFormatIDRef", 0, 32, 0, 32),
            child("absoluteDistance", 0, 1, 0, 1),
            child("encodePackFormatIDRef", 0, 1, 0, 1),
            child("decodePackFormatIDRef", 0, 1, 0, 1),
            child("inputPackFormatIDRef", 0, 1, 0, 1),
            child("outputPackFormatIDRef", 0, 1, 0, 1),
        ],
    );
    validate_child_constraints(
        parsed,
        mode,
        &mut rules,
        "audioChannelFormat",
        "TECH3393-2025-CHANNEL-FORMAT-CHILDREN",
        "Table 35",
        &[
            child("audioBlockFormat", 1, usize::MAX, 1, usize::MAX),
            child("frequency", 0, 2, 0, 2),
        ],
    );
    validate_child_constraints(
        parsed,
        mode,
        &mut rules,
        "audioStreamFormat",
        "TECH3393-2025-STREAM-FORMAT-CHILDREN",
        "Table 49",
        &[
            child("audioChannelFormatIDRef", 1, 1, 1, 1),
            child("audioPackFormatIDRef", 0, 0, 0, 0),
            child("audioTrackFormatIDRef", 0, usize::MAX, 0, 0),
        ],
    );

    validate_name_and_label_lengths(parsed, &mut rules);
    validate_object_structure(parsed, &mut rules);
    validate_object_graph(parsed, &mut rules);
    validate_tag_groups(parsed, mode, &mut rules);
    rules
}

const fn child(
    name: &'static str,
    read_min: usize,
    read_max: usize,
    write_min: usize,
    write_max: usize,
) -> ChildConstraint {
    ChildConstraint {
        name,
        read_min,
        read_max,
        write_min,
        write_max,
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_required_attributes(
    parsed: &ParsedAdm,
    mode: ProductionProfileMode,
    rules: &mut Vec<AdmProfileRule>,
    element_name: &str,
    rule_id: &'static str,
    table: &'static str,
    read_required: &[&str],
    write_required: &[&str],
) {
    let required = if mode == ProductionProfileMode::Write {
        write_required
    } else {
        read_required
    };
    for element in parsed
        .elements
        .iter()
        .filter(|element| element.name == element_name)
    {
        let missing = required
            .iter()
            .filter(|attribute| !element.attributes.contains_key(**attribute))
            .copied()
            .collect::<Vec<_>>();
        rules.push(AdmProfileRule {
            rule_id,
            path: element.path.clone(),
            requirement: format!(
                "Tech 3393:2025 {table}: required attributes: {}",
                required.join(", ")
            ),
            observed: if missing.is_empty() {
                "all required attributes present".into()
            } else {
                format!("missing: {}", missing.join(", "))
            },
            passed: missing.is_empty(),
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_format_attributes(
    parsed: &ParsedAdm,
    mode: ProductionProfileMode,
    rules: &mut Vec<AdmProfileRule>,
    element_name: &str,
    id_attribute: &str,
    name_attribute: &str,
    rule_id: &'static str,
    table: &'static str,
) {
    for element in parsed
        .elements
        .iter()
        .filter(|element| element.name == element_name)
    {
        let id_present = element.attributes.contains_key(id_attribute);
        let name_present = element.attributes.contains_key(name_attribute);
        let label = element.attributes.get("typeLabel");
        let definition = element.attributes.get("typeDefinition");
        let type_present = if mode == ProductionProfileMode::Write {
            label.is_some()
        } else {
            label.is_some() || definition.is_some()
        };
        let known_label = label.is_none_or(|value| {
            matches!(value.as_str(), "0001" | "0002" | "0003" | "0004" | "0005")
        });
        let known_definition = definition.is_none_or(|value| {
            matches!(
                value.as_str(),
                "DirectSpeakers" | "Matrix" | "Objects" | "HOA" | "Binaural" | "PCM"
            )
        });
        rules.push(AdmProfileRule {
            rule_id,
            path: element.path.clone(),
            requirement: format!(
                "Tech 3393:2025 {table}: ID and name are required; a published type shall be identified"
            ),
            observed: format!(
                "ID={}, name={}, typeLabel={}, typeDefinition={}",
                present(id_present),
                present(name_present),
                label.map_or("not present", String::as_str),
                definition.map_or("not present", String::as_str),
            ),
            passed: id_present
                && name_present
                && type_present
                && known_label
                && known_definition,
        });
    }
}

fn validate_child_constraints(
    parsed: &ParsedAdm,
    mode: ProductionProfileMode,
    rules: &mut Vec<AdmProfileRule>,
    element_name: &str,
    rule_id: &'static str,
    tables: &'static str,
    constraints: &[ChildConstraint],
) {
    for element in parsed
        .elements
        .iter()
        .filter(|element| element.name == element_name)
    {
        let violations = constraints
            .iter()
            .filter_map(|constraint| {
                let count = element.children.get(constraint.name).copied().unwrap_or(0);
                let (minimum, maximum) = if mode == ProductionProfileMode::Write {
                    (constraint.write_min, constraint.write_max)
                } else {
                    (constraint.read_min, constraint.read_max)
                };
                (!(minimum..=maximum).contains(&count)).then(|| {
                    if maximum == usize::MAX {
                        format!("{}={count}, expected at least {minimum}", constraint.name)
                    } else {
                        format!(
                            "{}={count}, expected {minimum}..={maximum}",
                            constraint.name
                        )
                    }
                })
            })
            .collect::<Vec<_>>();
        rules.push(AdmProfileRule {
            rule_id,
            path: element.path.clone(),
            requirement: format!("Tech 3393:2025 {tables}: child-element cardinalities"),
            observed: if violations.is_empty() {
                "all constrained child cardinalities valid".into()
            } else {
                violations.join("; ")
            },
            passed: violations.is_empty(),
        });
    }
}

fn validate_track_uid_children(
    parsed: &ParsedAdm,
    mode: ProductionProfileMode,
    rules: &mut Vec<AdmProfileRule>,
) {
    for element in parsed
        .elements
        .iter()
        .filter(|element| element.name == "audioTrackUID")
    {
        let track = child_count(element, "audioTrackFormatIDRef");
        let channel = child_count(element, "audioChannelFormatIDRef");
        let pack = child_count(element, "audioPackFormatIDRef");
        let passed = if mode == ProductionProfileMode::Write {
            track == 0 && channel == 1 && pack == 1
        } else {
            track + channel == 1 && pack == 1
        };
        rules.push(AdmProfileRule {
            rule_id: "TECH3393-2025-TRACK-UID-CHILDREN",
            path: element.path.clone(),
            requirement: match mode {
                ProductionProfileMode::Read => "Tech 3393:2025 Table 29: exactly one track/channel-format reference and one pack-format reference".into(),
                ProductionProfileMode::Write => "Tech 3393:2025 Table 29: exactly one channel-format reference, no track-format reference, and one pack-format reference".into(),
            },
            observed: format!("track={track}, channel={channel}, pack={pack}"),
            passed,
        });
    }
}

fn validate_name_and_label_lengths(parsed: &ParsedAdm, rules: &mut Vec<AdmProfileRule>) {
    let mut invalid_names = Vec::new();
    for element in &parsed.elements {
        for (attribute, value) in &element.attributes {
            if attribute.ends_with("Name") && value.chars().count() > 128 {
                invalid_names.push(format!(
                    "{}/@{}={} chars",
                    element.path,
                    attribute,
                    value.chars().count()
                ));
            }
        }
    }
    rules.push(AdmProfileRule {
        rule_id: "TECH3393-2025-NAME-LENGTH",
        path: "/audioFormatExtended".into(),
        requirement: "Tech 3393:2025 Table 3: ADM names shall not exceed 128 characters".into(),
        observed: if invalid_names.is_empty() {
            "all observed names are at most 128 characters".into()
        } else {
            invalid_names.join("; ")
        },
        passed: invalid_names.is_empty(),
    });

    let invalid_labels = parsed
        .elements
        .iter()
        .filter(|element| {
            matches!(
                element.name.as_str(),
                "audioProgrammeLabel"
                    | "audioContentLabel"
                    | "audioObjectLabel"
                    | "audioComplementaryObjectGroupLabel"
            ) && element.text.chars().count() > 256
        })
        .map(|element| format!("{}={} chars", element.path, element.text.chars().count()))
        .collect::<Vec<_>>();
    rules.push(AdmProfileRule {
        rule_id: "TECH3393-2025-LABEL-LENGTH",
        path: "/audioFormatExtended".into(),
        requirement: "Tech 3393:2025 Tables 5, 16, and 23: ADM labels shall not exceed 256 characters".into(),
        observed: if invalid_labels.is_empty() {
            "all observed labels are at most 256 characters".into()
        } else {
            invalid_labels.join("; ")
        },
        passed: invalid_labels.is_empty(),
    });
}

fn validate_object_structure(parsed: &ParsedAdm, rules: &mut Vec<AdmProfileRule>) {
    for element in parsed
        .elements
        .iter()
        .filter(|element| element.name == "audioObject")
    {
        let packs = child_count(element, "audioPackFormatIDRef");
        let objects = child_count(element, "audioObjectIDRef");
        let tracks = child_count(element, "audioTrackUIDRef");
        let passed = (packs == 1 && objects == 0 && tracks > 0)
            || (packs == 0 && objects > 0 && tracks == 0);
        rules.push(AdmProfileRule {
            rule_id: "TECH3393-2025-OBJECT-REFERENCE-MODE",
            path: element.path.clone(),
            requirement: "Tech 3393:2025 section 2.2.3: an audioObject shall reference one pack plus tracks, or child objects, but not both".into(),
            observed: format!("pack={packs}, object={objects}, trackUID={tracks}"),
            passed,
        });
    }
}

fn validate_object_graph(parsed: &ParsedAdm, rules: &mut Vec<AdmProfileRule>) {
    let objects = parsed
        .elements
        .iter()
        .enumerate()
        .filter(|(_, element)| element.name == "audioObject")
        .filter_map(|(index, element)| {
            element
                .attributes
                .get("audioObjectID")
                .map(|id| (id.as_str(), index))
        })
        .collect::<HashMap<_, _>>();
    let mut maximum_depth = 0;
    let mut cyclic = Vec::new();
    for id in objects.keys().copied() {
        let mut visiting = HashSet::new();
        match object_depth(parsed, &objects, id, &mut visiting) {
            Ok(depth) => maximum_depth = maximum_depth.max(depth),
            Err(cycle) => cyclic.push(cycle),
        }
    }
    cyclic.sort();
    cyclic.dedup();
    rules.push(AdmProfileRule {
        rule_id: "TECH3393-2025-OBJECT-NESTING",
        path: "/audioFormatExtended/audioObject".into(),
        requirement: "Tech 3393:2025 Tables 1 and 23: object references shall be acyclic and nesting shall not exceed six layers".into(),
        observed: if cyclic.is_empty() {
            format!("maximum nesting depth {maximum_depth}")
        } else {
            format!("cycle(s): {}", cyclic.join(", "))
        },
        passed: cyclic.is_empty() && maximum_depth <= 6,
    });
}

fn object_depth<'a>(
    parsed: &'a ParsedAdm,
    objects: &HashMap<&'a str, usize>,
    id: &'a str,
    visiting: &mut HashSet<&'a str>,
) -> Result<usize, String> {
    if !visiting.insert(id) {
        return Err(id.to_owned());
    }
    let Some(index) = objects.get(id).copied() else {
        visiting.remove(id);
        return Ok(1);
    };
    let children = parsed
        .elements
        .iter()
        .filter(|element| element.parent == Some(index) && element.name == "audioObjectIDRef")
        .map(|element| element.text.as_str())
        .filter(|reference| objects.contains_key(reference))
        .collect::<Vec<_>>();
    let mut depth = 1;
    for child in children {
        depth = depth.max(1 + object_depth(parsed, objects, child, visiting)?);
    }
    visiting.remove(id);
    Ok(depth)
}

fn validate_tag_groups(
    parsed: &ParsedAdm,
    mode: ProductionProfileMode,
    rules: &mut Vec<AdmProfileRule>,
) {
    let groups = parsed
        .elements
        .iter()
        .enumerate()
        .filter(|(_, element)| element.name == "tagGroup")
        .collect::<Vec<_>>();
    let mut layer_groups = 0;
    let mut other_groups = 0;
    for (index, group) in &groups {
        let tags = parsed
            .elements
            .iter()
            .filter(|element| element.parent == Some(*index) && element.name == "tag")
            .collect::<Vec<_>>();
        let layer = tags.iter().any(|tag| {
            tag.attributes.get("class").map(String::as_str) == Some("urn:profile:production:Layer")
        });
        if layer {
            layer_groups += 1;
        } else {
            other_groups += 1;
        }
        let references = child_count(group, "audioProgrammeIDRef")
            + child_count(group, "audioContentIDRef")
            + child_count(group, "audioObjectIDRef");
        let passed = tags.len() == 1
            && references > 0
            && child_count(group, "audioProgrammeIDRef") <= 1
            && child_count(group, "audioContentIDRef") <= 128
            && child_count(group, "audioObjectIDRef") <= 128;
        rules.push(AdmProfileRule {
            rule_id: "TECH3393-2025-TAG-GROUP-CHILDREN",
            path: group.path.clone(),
            requirement: "Tech 3393:2025 Tables 55 and 56: one tag and at least one bounded ADM reference per tagGroup".into(),
            observed: format!("tags={}, ADM references={references}", tags.len()),
            passed,
        });
    }
    let minimum_layers = usize::from(mode == ProductionProfileMode::Write);
    rules.push(AdmProfileRule {
        rule_id: "TECH3393-2025-TAG-GROUP-COUNT",
        path: "/audioFormatExtended/tagList".into(),
        requirement: format!(
            "Tech 3393:2025 Table 54: {minimum_layers}..=8 Layer tagGroup(s) and 0..=8 other tagGroup(s)"
        ),
        observed: format!("Layer={layer_groups}, other={other_groups}"),
        passed: (minimum_layers..=8).contains(&layer_groups) && other_groups <= 8,
    });
}

fn child_count(element: &ParsedElement, name: &str) -> usize {
    element.children.get(name).copied().unwrap_or(0)
}

fn present(value: bool) -> &'static str {
    if value {
        "present"
    } else {
        "not present"
    }
}

fn parse_adm(xml: &[u8]) -> Result<ParsedAdm, String> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut parsed = ParsedAdm::default();
    let mut stack = Vec::<String>::new();
    let mut active_profiles = Vec::<(usize, usize)>::new();
    let mut active_tracks = Vec::<(usize, usize)>::new();
    let mut active_tag_groups = Vec::<(usize, usize)>::new();
    let mut element_stack = Vec::<usize>::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                let name = local_name(element.name().as_ref());
                stack.push(name.clone());
                let index = observe_element(
                    &element,
                    &stack,
                    element_stack.last().copied(),
                    &mut parsed,
                    &mut active_profiles,
                    &mut active_tracks,
                    &mut active_tag_groups,
                )?;
                element_stack.push(index);
            }
            Ok(Event::Empty(element)) => {
                let name = local_name(element.name().as_ref());
                stack.push(name);
                observe_element(
                    &element,
                    &stack,
                    element_stack.last().copied(),
                    &mut parsed,
                    &mut active_profiles,
                    &mut active_tracks,
                    &mut active_tag_groups,
                )?;
                close_depth(
                    stack.len(),
                    &mut active_profiles,
                    &mut active_tracks,
                    &mut active_tag_groups,
                );
                stack.pop();
            }
            Ok(Event::Text(text)) => {
                let value = text.as_ref().trim().to_owned();
                if let Some(index) = element_stack.last() {
                    parsed.elements[*index].text.push_str(&value);
                }
                if let Some((_, index)) = active_profiles.last() {
                    parsed.profiles[*index].text.push_str(&value);
                }
                if stack.last().is_some_and(|name| name.ends_with("IDRef")) && !value.is_empty() {
                    parsed.references.push((xml_path(&stack), value));
                }
            }
            Ok(Event::End(_)) => {
                close_depth(
                    stack.len(),
                    &mut active_profiles,
                    &mut active_tracks,
                    &mut active_tag_groups,
                );
                stack.pop();
                element_stack.pop();
            }
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
    if !stack.is_empty() || !element_stack.is_empty() {
        return Err("XML ended with unclosed elements".into());
    }
    Ok(parsed)
}

fn observe_element(
    element: &quick_xml::events::BytesStart<'_>,
    stack: &[String],
    parent: Option<usize>,
    parsed: &mut ParsedAdm,
    active_profiles: &mut Vec<(usize, usize)>,
    active_tracks: &mut Vec<(usize, usize)>,
    active_tag_groups: &mut Vec<(usize, usize)>,
) -> Result<usize, String> {
    let name = stack.last().map(String::as_str).unwrap_or_default();
    let observed_name = canonical_element_name(name);
    let path = xml_path(stack);
    let mut attributes = HashMap::new();
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| format!("XML attribute at {path}: {error}"))?;
        let key = local_name(attribute.key.as_ref());
        let value = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|error| format!("XML attribute value at {path}: {error}"))?
            .into_owned();
        if key.ends_with("ID") || key == "UID" {
            parsed.ids.push(ParsedId {
                path: format!("{path}/@{key}"),
                element: name.to_owned(),
                value: value.clone(),
            });
        } else if key.ends_with("IDRef") || key.ends_with("IDRefs") {
            parsed.references.extend(
                value
                    .split_whitespace()
                    .map(|reference| (path.clone(), reference.to_owned())),
            );
        }
        if matches!(
            key.as_str(),
            "start" | "duration" | "rtime" | "interpolationLength"
        ) {
            parsed
                .time_values
                .push((format!("{path}/@{key}"), value.clone()));
        }
        attributes.insert(key, value);
    }
    let index = parsed.elements.len();
    if let Some(parent) = parent {
        *parsed.elements[parent]
            .children
            .entry(observed_name.to_owned())
            .or_default() += 1;
    }
    parsed.elements.push(ParsedElement {
        name: observed_name.to_owned(),
        path: path.clone(),
        parent,
        attributes: attributes.clone(),
        children: HashMap::new(),
        text: String::new(),
    });
    if name == "audioFormatExtended" {
        parsed
            .roots
            .push((path, attributes.get("version").cloned()));
    } else if name == "profileList" {
        parsed.profile_lists += 1;
    } else if name == "profile" {
        let index = parsed.profiles.len();
        parsed.profiles.push(ParsedProfile {
            path,
            text: String::new(),
            attributes,
        });
        active_profiles.push((stack.len(), index));
    } else if name == "audioTrackFormat" {
        let index = parsed.track_format_stream_refs.len();
        parsed.track_format_stream_refs.push((path, 0));
        active_tracks.push((stack.len(), index));
    } else if name == "audioStreamFormatIDRef" {
        if let Some((_, index)) = active_tracks.last() {
            parsed.track_format_stream_refs[*index].1 += 1;
        }
    } else if name == "tagList" {
        parsed.tag_lists += 1;
    } else if name == "tagGroup" {
        let index = parsed.tag_groups.len();
        parsed.tag_groups.push((path, 0));
        active_tag_groups.push((stack.len(), index));
    } else if matches!(
        name,
        "audioProgrammeIDRef" | "audioContentIDRef" | "audioObjectIDRef"
    ) {
        if let Some((_, index)) = active_tag_groups.last() {
            parsed.tag_groups[*index].1 += 1;
        }
    } else if name == "audioMXFLookUp" {
        parsed.deprecated_mxf_lookups.push(path);
    }
    Ok(index)
}

fn close_depth(
    depth: usize,
    profiles: &mut Vec<(usize, usize)>,
    tracks: &mut Vec<(usize, usize)>,
    tag_groups: &mut Vec<(usize, usize)>,
) {
    if profiles.last().is_some_and(|(start, _)| *start == depth) {
        profiles.pop();
    }
    if tracks.last().is_some_and(|(start, _)| *start == depth) {
        tracks.pop();
    }
    if tag_groups.last().is_some_and(|(start, _)| *start == depth) {
        tag_groups.pop();
    }
}

fn local_name(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_owned()
}

fn canonical_element_name(name: &str) -> &str {
    if name.starts_with("audioBlockFormat") {
        "audioBlockFormat"
    } else {
        name
    }
}

fn xml_path(stack: &[String]) -> String {
    format!("/{}", stack.join("/"))
}

fn duplicate_ids(ids: &[ParsedId]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut duplicates = ids
        .iter()
        .filter_map(|id| (!seen.insert(id.value.as_str())).then_some(id.value.clone()))
        .collect::<Vec<_>>();
    duplicates.sort();
    duplicates.dedup();
    duplicates
}

fn is_production_profile_adm_version(value: &str) -> bool {
    matches!(value, "ITU-R_BS.2076-2" | "ITU-R_BS.2076-3")
}

fn valid_adm_id(element: &str, value: &str) -> bool {
    let specification: Option<(&str, &[usize])> = match element {
        "audioPackFormat" => Some(("AP", &[8])),
        "audioChannelFormat" => Some(("AC", &[8])),
        element if element.starts_with("audioBlockFormat") => Some(("AB", &[8, 8])),
        "audioStreamFormat" => Some(("AS", &[8])),
        "audioTrackFormat" => Some(("AT", &[8, 2])),
        "audioProgramme" => Some(("APR", &[4])),
        "audioContent" => Some(("ACO", &[4])),
        "audioObject" => Some(("AO", &[4])),
        "alternativeValueSet" => Some(("AVS", &[4, 4])),
        "audioTrackUID" => Some(("ATU", &[8])),
        _ => None,
    };
    let Some((prefix, lengths)) = specification else {
        return true;
    };
    let Some(remainder) = value
        .strip_prefix(prefix)
        .and_then(|value| value.strip_prefix('_'))
    else {
        return false;
    };
    let segments = remainder.split('_').collect::<Vec<_>>();
    segments.len() == lengths.len()
        && segments.iter().zip(lengths).all(|(segment, length)| {
            segment.len() == *length && segment.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

fn requires_local_definition(reference: &str) -> bool {
    ["APR_", "ACO_", "AO_", "AVS_", "ATU_"]
        .iter()
        .any(|prefix| reference.starts_with(prefix))
}

fn valid_adm_time(value: &str) -> bool {
    if value.is_empty() || value.starts_with('-') {
        return false;
    }
    if let Some((numerator_part, denominator)) = value.split_once('S') {
        if denominator.is_empty()
            || !denominator.bytes().all(|byte| byte.is_ascii_digit())
            || denominator.bytes().all(|byte| byte == b'0')
            || numerator_part.contains('S')
        {
            return false;
        }
        let numerator = numerator_part.rsplit('.').next().unwrap_or_default();
        if numerator.is_empty() || !numerator.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        if numerator_part.contains(':') {
            if !valid_clock_time(numerator_part) || numerator.len() != denominator.len() {
                return false;
            }
            return decimal_digits_less_than(numerator, denominator);
        }
        return numerator_part.bytes().all(|byte| byte.is_ascii_digit());
    }
    if value.contains(':') {
        valid_clock_time(value)
    } else {
        valid_decimal_seconds(value)
    }
}

fn valid_clock_time(value: &str) -> bool {
    let parts = value.split(':').collect::<Vec<_>>();
    if parts.len() != 3
        || parts[0].is_empty()
        || !parts[0].bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    let Ok(minutes) = parts[1].parse::<u8>() else {
        return false;
    };
    let seconds = parts[2].split('.').next().unwrap_or_default();
    let Ok(seconds) = seconds.parse::<u8>() else {
        return false;
    };
    minutes < 60 && seconds < 60 && valid_decimal_seconds(parts[2])
}

fn valid_decimal_seconds(value: &str) -> bool {
    let mut parts = value.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next();
    parts.next().is_none()
        && !whole.is_empty()
        && whole.bytes().all(|byte| byte.is_ascii_digit())
        && fraction.is_none_or(|fraction| {
            !fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn decimal_digits_less_than(left: &str, right: &str) -> bool {
    left.len() < right.len() || (left.len() == right.len() && left < right)
}

fn validate_chna(input: &Path, parsed: &ParsedAdm) -> Result<Vec<AdmProfileRule>, String> {
    let Some(body) = metadata::read_wave_chunk(input, *b"chna")? else {
        return Ok(vec![AdmProfileRule {
            rule_id: "BS2076-3-CHNA-REQUIRED",
            path: "/chna".into(),
            requirement: "ADM BW64 shall contain a chna chunk".into(),
            observed: "not present".into(),
            passed: false,
        }]);
    };
    if body.len() < 4 {
        return Ok(vec![AdmProfileRule {
            rule_id: "BS2076-3-CHNA-STRUCTURE",
            path: "/chna".into(),
            requirement: "chna shall contain its four-byte header and every declared audioID"
                .into(),
            observed: format!("{} byte(s)", body.len()),
            passed: false,
        }]);
    }
    let num_tracks = u16::from_le_bytes(body[0..2].try_into().unwrap());
    let num_uids = u16::from_le_bytes(body[2..4].try_into().unwrap());
    let records_bytes = body.len() - 4;
    let capacity = records_bytes / 40;
    let used_size = usize::from(num_uids).saturating_mul(40);
    let structure_passed = records_bytes.is_multiple_of(40)
        && capacity >= usize::from(num_uids)
        && body
            .get(4 + used_size..)
            .is_some_and(|unused| unused.iter().all(|byte| *byte == 0));
    let mut rules = vec![AdmProfileRule {
        rule_id: "BS2076-3-CHNA-STRUCTURE",
        path: "/chna".into(),
        requirement:
            "chna shall hold every declared 40-byte audioID and zero-fill unused records".into(),
        observed: format!(
            "{} byte(s), {num_tracks} track(s), {num_uids} used audioID(s), {capacity} record slot(s)",
            body.len(),
        ),
        passed: structure_passed,
    }];
    if !structure_passed {
        return Ok(rules);
    }

    // CHNA validation only needs the PCM track count. Speaker semantics are
    // supplied by ADM rather than a WAVE channel mask, so maskless
    // multichannel BW64 must remain inspectable here.
    let channels = crate::wav::WavReader::probe_with_layout(input)
        .map_err(|error| format!("probe {} for chna validation: {error}", input.display()))?
        .0
        .channels;
    rules.push(AdmProfileRule {
        rule_id: "BS2076-3-CHNA-TRACK-COUNT",
        path: "/chna/@numTracks".into(),
        requirement: "chna numTracks shall equal the number of PCM tracks".into(),
        observed: format!("{num_tracks} declared, {channels} PCM track(s)"),
        passed: num_tracks == channels,
    });

    let mut track_indices = Vec::with_capacity(usize::from(num_uids));
    let mut chna_uids = Vec::with_capacity(usize::from(num_uids));
    for entry in body[4..4 + used_size].chunks_exact(40) {
        track_indices.push(u16::from_le_bytes(entry[..2].try_into().unwrap()));
        chna_uids.push(
            String::from_utf8_lossy(&entry[2..14])
                .trim_end_matches('\0')
                .trim_end()
                .to_owned(),
        );
    }
    let indices_valid = track_indices
        .iter()
        .all(|index| (1..=channels).contains(index));
    let all_tracks_described = (1..=channels).all(|track| track_indices.contains(&track));
    let unique_uids = chna_uids.iter().collect::<HashSet<_>>().len() == chna_uids.len();
    let uids_valid = chna_uids
        .iter()
        .all(|uid| valid_adm_id("audioTrackUID", uid));
    rules.push(AdmProfileRule {
        rule_id: "BS2076-3-CHNA-AUDIO-ID-UNIQUE",
        path: "/chna/audioID".into(),
        requirement:
            "audioID records shall cover every PCM track with unique, syntactically valid UIDs"
                .into(),
        observed: format!(
            "track indices [{}], UIDs [{}]",
            track_indices
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            chna_uids.join(", ")
        ),
        passed: indices_valid && all_tracks_described && unique_uids && uids_valid,
    });

    let xml_uids = parsed
        .ids
        .iter()
        .filter(|id| id.element == "audioTrackUID")
        .map(|id| id.value.as_str())
        .collect::<HashSet<_>>();
    let chna_uid_set = chna_uids.iter().map(String::as_str).collect::<HashSet<_>>();
    rules.push(AdmProfileRule {
        rule_id: "BS2076-3-CHNA-UID-XCHECK",
        path: "/chna/audioID/UID".into(),
        requirement: "embedded audioTrackUID definitions shall match chna UID records".into(),
        observed: format!(
            "{} embedded UID(s), {} chna UID(s)",
            xml_uids.len(),
            chna_uid_set.len()
        ),
        passed: xml_uids.is_empty() || xml_uids == chna_uid_set,
    });
    Ok(rules)
}

fn validate_options(options: &ReferenceRendererOptions) -> Result<(), String> {
    if options.command.as_os_str().is_empty() {
        return Err("ADM renderer command cannot be empty".into());
    }
    if options.layout.trim().is_empty() {
        return Err("ADM render layout cannot be empty".into());
    }
    if options.profile_level > 2 {
        return Err("BS.2168 profile level must be 0, 1, or 2".into());
    }
    Ok(())
}

fn require_adm_chunks(input: &Path) -> Result<(), String> {
    if metadata::read_wave_chunk(input, *b"axml")?.is_none() {
        return Err("ADM reference rendering requires an axml chunk".into());
    }
    if metadata::read_wave_chunk(input, *b"chna")?.is_none() {
        return Err("ADM reference rendering requires a chna chunk".into());
    }
    Ok(())
}

fn write_config(path: &Path, value: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).expect("JSON values are serializable");
    fs::write(path, bytes).map_err(|error| format!("write {}: {error}", path.display()))
}

fn run_eat(
    options: &ReferenceRendererOptions,
    config: &Path,
    overrides: &[(&str, &std::ffi::OsStr)],
    stage: &str,
) -> Result<(), String> {
    let mut command = Command::new(&options.command);
    command.arg(config);
    for (name, value) in overrides {
        command.arg("-o").arg(name).arg(value);
    }
    let output = command.output().map_err(|error| {
        format!(
            "start ADM renderer {} for {stage}: {error}; install the EBU ADM Toolbox or pass --adm-renderer",
            options.command.display()
        )
    })?;
    check_output(output, &options.command, stage)
}

fn check_output(output: Output, command: &Path, stage: &str) -> Result<(), String> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    Err(format!(
        "ADM renderer {} failed during {stage} ({}): {}",
        command.display(),
        output.status,
        if detail.is_empty() {
            "no diagnostic output"
        } else {
            detail
        }
    ))
}

fn validation_config(level: u8) -> Value {
    json!({
        "version": 0,
        "processes": [
            {
                "name": "input",
                "type": "read_adm_bw64",
                "out_ports": ["out_axml"]
            },
            {
                "name": "validate",
                "type": "validate",
                "in_ports": ["in_axml"],
                "parameters": {
                    "profile": {
                        "type": "itu_emission",
                        "level": level
                    }
                }
            }
        ]
    })
}

fn render_config_value() -> Value {
    json!({
        "version": 0,
        "processes": [
            {
                "name": "input",
                "type": "read_adm_bw64",
                "out_ports": ["out_axml"]
            },
            {
                "name": "add_block_rtimes",
                "type": "add_block_rtimes",
                "in_ports": ["in_axml"],
                "out_ports": ["out_axml"]
            },
            {
                "name": "render",
                "type": "render",
                "in_ports": ["in_axml"],
                "out_ports": ["out_samples"]
            },
            {
                "name": "output",
                "type": "write_bw64",
                "in_ports": ["in_samples"]
            }
        ],
        "connections": [
            ["input.out_samples", "render.in_samples"]
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{
        default_channel_roles, AudioBuffer, ChannelRole, PcmKind, WavContainer, WavWriter,
        WaveChunk,
    };

    fn write_adm_fixture(path: &Path, axml: &[u8]) {
        write_adm_fixture_with_unused_records(path, axml, 0);
    }

    fn write_adm_fixture_with_unused_records(path: &Path, axml: &[u8], unused_records: usize) {
        let buffer = AudioBuffer {
            sample_rate: 48_000,
            channels: 1,
            frames: 4_800,
            data: vec![vec![0.0; 4_800]],
            channel_roles: default_channel_roles(1),
            source_kind: PcmKind::F32,
        };
        let mut chna = Vec::with_capacity(44);
        chna.extend_from_slice(&1_u16.to_le_bytes());
        chna.extend_from_slice(&1_u16.to_le_bytes());
        chna.extend_from_slice(&1_u16.to_le_bytes());
        chna.extend_from_slice(b"ATU_00000001");
        chna.extend_from_slice(&[0; 14]);
        chna.extend_from_slice(&[0; 11]);
        chna.push(0);
        chna.resize(chna.len() + unused_records * 40, 0);
        WavWriter::write_with_metadata(
            path,
            &buffer,
            PcmKind::F32,
            false,
            WavContainer::Bw64,
            &[
                WaveChunk {
                    id: *b"axml",
                    body: axml.to_vec(),
                },
                WaveChunk {
                    id: *b"chna",
                    body: chna,
                },
            ],
        )
        .unwrap();
    }

    const VALID_TECH3393_AXML: &[u8] = br#"<audioFormatExtended version="ITU-R_BS.2076-3">
  <profileList>
    <profile profileName="EBU Production Profile" profileVersion="1.0" profileLevel="1">EBU Tech 3393</profile>
  </profileList>
  <tagList><tagGroup><tag class="urn:profile:production:Layer">1</tag><audioProgrammeIDRef>APR_1001</audioProgrammeIDRef></tagGroup></tagList>
  <audioProgramme audioProgrammeID="APR_1001" audioProgrammeName="Programme">
    <audioContentIDRef>ACO_1001</audioContentIDRef>
  </audioProgramme>
  <audioContent audioContentID="ACO_1001" audioContentName="Content">
    <audioObjectIDRef>AO_1001</audioObjectIDRef><dialogue>1</dialogue>
  </audioContent>
  <audioObject audioObjectID="AO_1001" audioObjectName="Object" importance="10" interact="0">
    <audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef>
    <audioTrackUIDRef>ATU_00000001</audioTrackUIDRef>
  </audioObject>
  <audioTrackUID UID="ATU_00000001">
    <audioChannelFormatIDRef>AC_00010001</audioChannelFormatIDRef>
    <audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef>
  </audioTrackUID>
</audioFormatExtended>"#;

    #[test]
    fn production_profile_write_mode_requires_tech3393_declaration() {
        let work = tempfile::tempdir().unwrap();
        let input = work.path().join("production.bw64");
        write_adm_fixture(&input, VALID_TECH3393_AXML);
        let result = validate_production_profile(&input, ProductionProfileMode::Write).unwrap();
        assert!(result.passed, "{:#?}", result.rules);
        assert_eq!(result.standard, "EBU Tech 3393");
        assert_eq!(result.profile_version, "1.0");
        assert_eq!(result.profile_level, "1");
    }

    #[test]
    fn chna_accepts_zero_filled_unused_record_capacity() {
        let work = tempfile::tempdir().unwrap();
        let input = work.path().join("reserved-chna-capacity.bw64");
        write_adm_fixture_with_unused_records(&input, VALID_TECH3393_AXML, 2);

        let result = validate_production_profile(&input, ProductionProfileMode::Write).unwrap();
        assert!(result.passed, "{:#?}", result.rules);
        assert!(result.rules.iter().any(|rule| {
            rule.rule_id == "BS2076-3-CHNA-STRUCTURE"
                && rule.passed
                && rule.observed.contains("3 record slot(s)")
        }));
    }

    #[test]
    fn chna_track_count_accepts_maskless_multichannel_bw64() {
        let work = tempfile::tempdir().unwrap();
        let input = work.path().join("maskless-adm.bw64");
        let channels = 6_u16;
        let buffer = AudioBuffer {
            sample_rate: 48_000,
            channels,
            frames: 8,
            data: vec![vec![0.0; 8]; usize::from(channels)],
            channel_roles: vec![ChannelRole::Main; usize::from(channels)],
            source_kind: PcmKind::F32,
        };
        let mut chna = Vec::with_capacity(4 + usize::from(channels) * 40);
        chna.extend_from_slice(&channels.to_le_bytes());
        chna.extend_from_slice(&channels.to_le_bytes());
        for track in 1..=channels {
            chna.extend_from_slice(&track.to_le_bytes());
            chna.extend_from_slice(format!("ATU_{track:08x}").as_bytes());
            chna.extend_from_slice(&[0; 14]);
            chna.extend_from_slice(&[0; 11]);
            chna.push(0);
        }
        WavWriter::write_with_metadata(
            &input,
            &buffer,
            PcmKind::F32,
            false,
            WavContainer::Bw64,
            &[WaveChunk {
                id: *b"chna",
                body: chna,
            }],
        )
        .unwrap();

        assert!(crate::wav::WavReader::probe(&input).is_err());
        let rules = validate_chna(&input, &ParsedAdm::default()).unwrap();
        assert!(rules.iter().all(|rule| rule.passed), "{rules:#?}");
    }

    #[test]
    fn production_profile_reports_rule_ids_and_paths() {
        let work = tempfile::tempdir().unwrap();
        let input = work.path().join("invalid.bw64");
        write_adm_fixture(
            &input,
            br#"<audioFormatExtended>
  <profileList><profile profileName="wrong">EBU Tech 3393</profile></profileList>
  <audioObject audioObjectID="AO_1001"/>
  <audioObject audioObjectID="AO_1001"/>
</audioFormatExtended>"#,
        );
        let result = validate_production_profile(&input, ProductionProfileMode::Write).unwrap();
        assert!(!result.passed);
        assert!(result.rules.iter().any(|rule| {
            rule.rule_id == "TECH3393-2025-PROFILE-NAME"
                && rule.path.ends_with("/@profileName")
                && !rule.passed
        }));
        assert!(result
            .rules
            .iter()
            .any(|rule| rule.rule_id == "TECH3393-ADM-ID-UNIQUE" && !rule.passed));
    }

    #[test]
    fn read_mode_allows_missing_profile_declaration_but_checks_track_reference() {
        let work = tempfile::tempdir().unwrap();
        let input = work.path().join("read.bw64");
        write_adm_fixture(
            &input,
            br#"<audioFormatExtended>
  <audioTrackFormat audioTrackFormatID="AT_00010001_01"/>
</audioFormatExtended>"#,
        );
        let result = validate_production_profile(&input, ProductionProfileMode::Read).unwrap();
        assert!(!result.passed);
        assert!(result.rules.iter().any(|rule| {
            rule.rule_id == "TECH3393-2025-TRACK-STREAM-REFERENCE" && !rule.passed
        }));
        assert!(result
            .rules
            .iter()
            .any(|rule| { rule.rule_id == "TECH3393-2025-PROFILE-IDENTIFIER" && rule.passed }));
    }

    #[test]
    fn bs2076_3_rules_validate_ids_times_tags_references_and_chna() {
        let work = tempfile::tempdir().unwrap();
        let input = work.path().join("bs2076-3.bw64");
        write_adm_fixture(
            &input,
            br#"<audioFormatExtended version="ITU-R_BS.2076-3">
  <profileList>
    <profile profileName="EBU Production Profile" profileVersion="1.0" profileLevel="1">EBU Tech 3393</profile>
  </profileList>
  <tagList><tagGroup><tag class="urn:profile:production:Layer">1</tag><audioProgrammeIDRef>APR_1001</audioProgrammeIDRef></tagGroup></tagList>
  <audioProgramme audioProgrammeID="APR_1001" audioProgrammeName="Programme"><audioContentIDRef>ACO_1001</audioContentIDRef></audioProgramme>
  <audioContent audioContentID="ACO_1001" audioContentName="Content"><audioObjectIDRef>AO_1001</audioObjectIDRef><dialogue>1</dialogue></audioContent>
  <audioObject audioObjectID="AO_1001" audioObjectName="Object" importance="10" interact="0" start="00:00:00.12000S48000" duration="00:00:01.00000">
    <audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef>
    <audioTrackUIDRef>ATU_00000001</audioTrackUIDRef>
  </audioObject>
  <audioTrackUID UID="ATU_00000001">
    <audioChannelFormatIDRef>AC_00010001</audioChannelFormatIDRef>
    <audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef>
  </audioTrackUID>
</audioFormatExtended>"#,
        );
        let result = validate_production_profile(&input, ProductionProfileMode::Write).unwrap();
        assert!(result.passed, "{:#?}", result.rules);
        for rule_id in [
            "TECH3393-2025-AFE-VERSION",
            "BS2076-3-ID-SYNTAX",
            "BS2076-3-TIME-FORMAT",
            "BS2076-3-TAG-GROUP-REFERENCE",
            "BS2076-3-LOCAL-REFERENCES",
            "BS2076-3-CHNA-UID-XCHECK",
        ] {
            assert!(result
                .rules
                .iter()
                .any(|rule| rule.rule_id == rule_id && rule.passed));
        }
    }

    #[test]
    fn bs2076_3_rules_reject_deprecated_invalid_and_unresolved_metadata() {
        let work = tempfile::tempdir().unwrap();
        let input = work.path().join("invalid-bs2076-3.bw64");
        write_adm_fixture(
            &input,
            br#"<audioFormatExtended version="ITU-R_BS.2076-99">
  <profileList>
    <profile profileName="EBU Production Profile" profileVersion="1.0" profileLevel="1">EBU Tech 3393</profile>
  </profileList>
  <tagList><tagGroup><tag>orphan</tag></tagGroup></tagList>
  <audioObject audioObjectID="not-an-id" start="00:61:00.00000">
    <audioTrackUIDRef>ATU_ffffffff</audioTrackUIDRef>
  </audioObject>
  <audioTrackUID UID="ATU_00000001"><audioMXFLookUp/></audioTrackUID>
</audioFormatExtended>"#,
        );
        let result = validate_production_profile(&input, ProductionProfileMode::Write).unwrap();
        assert!(!result.passed);
        for rule_id in [
            "TECH3393-2025-AFE-VERSION",
            "BS2076-3-ID-SYNTAX",
            "BS2076-3-TIME-FORMAT",
            "BS2076-3-TAG-GROUP-REFERENCE",
            "BS2076-3-DEPRECATED-MXF-LOOKUP",
            "BS2076-3-LOCAL-REFERENCES",
        ] {
            assert!(result
                .rules
                .iter()
                .any(|rule| rule.rule_id == rule_id && !rule.passed));
        }
    }

    #[test]
    fn validation_targets_the_requested_bs2168_level() {
        let config = validation_config(2);
        assert_eq!(
            config["processes"][1]["parameters"]["profile"],
            json!({"type": "itu_emission", "level": 2})
        );
    }

    #[test]
    fn render_graph_uses_the_bs2127_renderer_process() {
        let config = render_config_value();
        assert!(config["processes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|process| process["type"] == "render"));
        assert_eq!(
            config["connections"][0],
            json!(["input.out_samples", "render.in_samples"])
        );
    }

    #[test]
    fn rejects_invalid_profile_level_before_spawning() {
        let options = ReferenceRendererOptions {
            profile_level: 3,
            ..ReferenceRendererOptions::default()
        };
        assert_eq!(
            validate_options(&options).unwrap_err(),
            "BS.2168 profile level must be 0, 1, or 2"
        );
    }

    #[cfg(unix)]
    #[test]
    fn adapter_validates_renders_and_measures_external_output() {
        use std::os::unix::fs::PermissionsExt;

        let work = tempfile::tempdir().unwrap();
        let input = work.path().join("input.bw64");
        let retained = work.path().join("rendered.wav");
        let renderer = work.path().join("eat-process");
        let samples = (0..48_000)
            .map(|frame| {
                (2.0 * std::f32::consts::PI * 997.0 * frame as f32 / 48_000.0).sin() * 0.05
            })
            .collect::<Vec<_>>();
        let buffer = AudioBuffer {
            sample_rate: 48_000,
            channels: 1,
            frames: samples.len(),
            data: vec![samples],
            channel_roles: default_channel_roles(1),
            source_kind: PcmKind::F32,
        };
        WavWriter::write_with_metadata(
            &input,
            &buffer,
            PcmKind::F32,
            false,
            WavContainer::Bw64,
            &[
                WaveChunk {
                    id: *b"axml",
                    body: br#"<audioProgramme audioProgrammeID="APR_1001"/>"#.to_vec(),
                },
                WaveChunk {
                    id: *b"chna",
                    body: vec![1, 0, 1, 0],
                },
            ],
        )
        .unwrap();
        fs::write(
            &renderer,
            r#"#!/usr/bin/env sh
set -eu
printf '%s\n' "$*" >> "$0.log"
input=
output=
while [ "$#" -gt 0 ]; do
    if [ "$1" = "-o" ]; then
        key=$2
        value=$3
        shift 3
        [ "$key" = "input.path" ] && input=$value
        [ "$key" = "output.path" ] && output=$value
    else
        shift
    fi
done
[ -z "$output" ] || cp "$input" "$output"
"#,
        )
        .unwrap();
        fs::set_permissions(&renderer, fs::Permissions::from_mode(0o755)).unwrap();

        let result = validate_and_render(
            &input,
            Some(&retained),
            &ReferenceRendererOptions {
                command: renderer.clone(),
                layout: "0+1+0".into(),
                profile_level: 1,
                overwrite: false,
            },
        )
        .unwrap();
        assert!(result.analysis.lufs.is_finite());
        assert_eq!(result.layout, "0+1+0");
        assert_eq!(result.profile_level, 1);
        assert_eq!(result.output_path.as_deref(), Some(retained.as_path()));
        assert!(retained.is_file());
        let invocations = fs::read_to_string(renderer.with_extension("log")).unwrap();
        assert_eq!(invocations.lines().count(), 2);
        assert!(invocations.contains("render.layout 0+1+0"));
    }
}
