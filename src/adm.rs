//! Adapter for the EBU ADM Toolbox reference implementation.
//!
//! Forge deliberately delegates the full ITU-R BS.2127 rendering algorithm to
//! `eat-process` instead of approximating object, HOA, matrix, or binaural
//! rendering. The adapter validates the input against the ITU-R BS.2168
//! emission profile, renders it to a BS.2051 layout, and then measures the
//! rendered loudspeaker signals with Forge's BS.1770 engine.

use crate::metadata;
use crate::normalize::{self, Analysis};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub const RENDERER_STANDARD: &str = "ITU-R BS.2127-1";
pub const PROFILE_STANDARD: &str = "ITU-R BS.2168-0";

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
