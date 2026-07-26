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
use quick_xml::Reader;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub const RENDERER_STANDARD: &str = "ITU-R BS.2127-1";
pub const PROFILE_STANDARD: &str = "ITU-R BS.2168-0";
pub const PRODUCTION_PROFILE_STANDARD: &str = "EBU Tech 3393";
pub const PRODUCTION_PROFILE_NAME: &str = "EBU Production Profile";
pub const PRODUCTION_PROFILE_VERSION: &str = "1.0";
pub const PRODUCTION_PROFILE_LEVEL: &str = "1";
pub const PRODUCTION_VALIDATOR: &str = "forge-tech3393-core-1";

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
    pub standard: &'static str,
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
    profile_lists: usize,
    profiles: Vec<ParsedProfile>,
    track_format_stream_refs: Vec<(String, usize)>,
    ids: Vec<(String, String)>,
    references: Vec<(String, String)>,
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

    let profile_list_pass = match mode {
        ProductionProfileMode::Read => parsed.profile_lists <= 1,
        ProductionProfileMode::Write => parsed.profile_lists == 1,
    };
    rules.push(AdmProfileRule {
        rule_id: "TECH3393-2.2.10-PROFILE-LIST",
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
        rule_id: "TECH3393-TABLE50-PROFILE-COUNT",
        path: "/audioFormatExtended/profileList/profile".into(),
        requirement: match mode {
            ProductionProfileMode::Read => "zero to eight profile elements".into(),
            ProductionProfileMode::Write => "one to eight profile elements".into(),
        },
        observed: format!("{} profile element(s)", parsed.profiles.len()),
        passed: count_pass,
    });

    let production = parsed
        .profiles
        .iter()
        .find(|profile| profile.text.trim() == PRODUCTION_PROFILE_STANDARD);
    rules.push(AdmProfileRule {
        rule_id: "TECH3393-TABLE50-PROFILE-IDENTIFIER",
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
                "TECH3393-TABLE51-PROFILE-NAME",
                "profileName",
                PRODUCTION_PROFILE_NAME,
            ),
            (
                "TECH3393-TABLE51-PROFILE-VERSION",
                "profileVersion",
                PRODUCTION_PROFILE_VERSION,
            ),
            (
                "TECH3393-TABLE51-PROFILE-LEVEL",
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

    if mode == ProductionProfileMode::Read {
        for (path, count) in parsed.track_format_stream_refs {
            rules.push(AdmProfileRule {
                rule_id: "TECH3393-TABLE49-STREAM-REFERENCE",
                path,
                requirement: "audioTrackFormat shall contain exactly one audioStreamFormatIDRef"
                    .into(),
                observed: format!("{count} audioStreamFormatIDRef element(s)"),
                passed: count == 1,
            });
        }
    }

    // Retain the observed reference count in an auditable rule without
    // rejecting common-definition references that are valid outside axml.
    rules.push(AdmProfileRule {
        rule_id: "TECH3393-ADM-REFERENCES-OBSERVED",
        path: "/audioFormatExtended".into(),
        requirement: "report ADM ID references for downstream validation".into(),
        observed: format!("{} ID reference(s)", parsed.references.len()),
        passed: true,
    });
    Ok(profile_result(mode, rules))
}

fn profile_result(
    mode: ProductionProfileMode,
    rules: Vec<AdmProfileRule>,
) -> ProductionProfileResult {
    ProductionProfileResult {
        standard: PRODUCTION_PROFILE_STANDARD,
        profile_name: PRODUCTION_PROFILE_NAME,
        profile_version: PRODUCTION_PROFILE_VERSION,
        profile_level: PRODUCTION_PROFILE_LEVEL,
        mode,
        validator: PRODUCTION_VALIDATOR,
        passed: rules.iter().all(|rule| rule.passed),
        rules,
    }
}

fn parse_adm(xml: &[u8]) -> Result<ParsedAdm, String> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut parsed = ParsedAdm::default();
    let mut stack = Vec::<String>::new();
    let mut active_profiles = Vec::<(usize, usize)>::new();
    let mut active_tracks = Vec::<(usize, usize)>::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                let name = local_name(element.name().as_ref());
                stack.push(name.clone());
                observe_element(
                    &reader,
                    &element,
                    &stack,
                    &mut parsed,
                    &mut active_profiles,
                    &mut active_tracks,
                )?;
            }
            Ok(Event::Empty(element)) => {
                let name = local_name(element.name().as_ref());
                stack.push(name);
                observe_element(
                    &reader,
                    &element,
                    &stack,
                    &mut parsed,
                    &mut active_profiles,
                    &mut active_tracks,
                )?;
                close_depth(stack.len(), &mut active_profiles, &mut active_tracks);
                stack.pop();
            }
            Ok(Event::Text(text)) => {
                let value = String::from_utf8_lossy(text.as_ref()).trim().to_owned();
                if let Some((_, index)) = active_profiles.last() {
                    parsed.profiles[*index].text.push_str(&value);
                }
                if stack.last().is_some_and(|name| name.ends_with("IDRef")) && !value.is_empty() {
                    parsed.references.push((xml_path(&stack), value));
                }
            }
            Ok(Event::End(_)) => {
                close_depth(stack.len(), &mut active_profiles, &mut active_tracks);
                stack.pop();
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
    if !stack.is_empty() {
        return Err("XML ended with unclosed elements".into());
    }
    Ok(parsed)
}

fn observe_element(
    reader: &Reader<&[u8]>,
    element: &quick_xml::events::BytesStart<'_>,
    stack: &[String],
    parsed: &mut ParsedAdm,
    active_profiles: &mut Vec<(usize, usize)>,
    active_tracks: &mut Vec<(usize, usize)>,
) -> Result<(), String> {
    let name = stack.last().map(String::as_str).unwrap_or_default();
    let path = xml_path(stack);
    let mut attributes = HashMap::new();
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| format!("XML attribute at {path}: {error}"))?;
        let key = local_name(attribute.key.as_ref());
        let value = attribute
            .decode_and_unescape_value(reader.decoder())
            .map_err(|error| format!("XML attribute value at {path}: {error}"))?
            .into_owned();
        if key.ends_with("ID") {
            parsed.ids.push((path.clone(), value.clone()));
        } else if key.ends_with("IDRef") || key.ends_with("IDRefs") {
            parsed.references.extend(
                value
                    .split_whitespace()
                    .map(|reference| (path.clone(), reference.to_owned())),
            );
        }
        attributes.insert(key, value);
    }
    if name == "profileList" {
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
    }
    Ok(())
}

fn close_depth(depth: usize, profiles: &mut Vec<(usize, usize)>, tracks: &mut Vec<(usize, usize)>) {
    if profiles.last().is_some_and(|(start, _)| *start == depth) {
        profiles.pop();
    }
    if tracks.last().is_some_and(|(start, _)| *start == depth) {
        tracks.pop();
    }
}

fn local_name(name: &[u8]) -> String {
    let name = String::from_utf8_lossy(name);
    name.rsplit(':').next().unwrap_or(&name).to_owned()
}

fn xml_path(stack: &[String]) -> String {
    format!("/{}", stack.join("/"))
}

fn duplicate_ids(ids: &[(String, String)]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut duplicates = ids
        .iter()
        .filter_map(|(_, id)| (!seen.insert(id.as_str())).then_some(id.clone()))
        .collect::<Vec<_>>();
    duplicates.sort();
    duplicates.dedup();
    duplicates
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
        default_channel_roles, AudioBuffer, PcmKind, WavContainer, WavWriter, WaveChunk,
    };

    fn write_adm_fixture(path: &Path, axml: &[u8]) {
        let buffer = AudioBuffer {
            sample_rate: 48_000,
            channels: 1,
            frames: 4_800,
            data: vec![vec![0.0; 4_800]],
            channel_roles: default_channel_roles(1),
            source_kind: PcmKind::F32,
        };
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
                    body: vec![1, 0, 1, 0],
                },
            ],
        )
        .unwrap();
    }

    #[test]
    fn production_profile_write_mode_requires_tech3393_declaration() {
        let work = tempfile::tempdir().unwrap();
        let input = work.path().join("production.bw64");
        write_adm_fixture(
            &input,
            br#"<audioFormatExtended>
  <profileList>
    <profile profileName="EBU Production Profile" profileVersion="1.0" profileLevel="1">EBU Tech 3393</profile>
  </profileList>
  <audioTrackFormat audioTrackFormatID="AT_00010001_01">
    <audioStreamFormatIDRef>AS_00010001</audioStreamFormatIDRef>
  </audioTrackFormat>
</audioFormatExtended>"#,
        );
        let result = validate_production_profile(&input, ProductionProfileMode::Write).unwrap();
        assert!(result.passed, "{:#?}", result.rules);
        assert_eq!(result.standard, "EBU Tech 3393");
        assert_eq!(result.profile_version, "1.0");
        assert_eq!(result.profile_level, "1");
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
            rule.rule_id == "TECH3393-TABLE51-PROFILE-NAME"
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
        assert!(result
            .rules
            .iter()
            .any(|rule| { rule.rule_id == "TECH3393-TABLE49-STREAM-REFERENCE" && !rule.passed }));
        assert!(result
            .rules
            .iter()
            .any(|rule| { rule.rule_id == "TECH3393-TABLE50-PROFILE-IDENTIFIER" && rule.passed }));
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
