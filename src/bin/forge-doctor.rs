use clap::Parser;
use serde::Serialize;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const SCHEMA_VERSION: &str = "forge-doctor-v1";
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Parser)]
#[command(
    name = "forge-doctor",
    version,
    about = "Report the capabilities available in this Forge build and runtime"
)]
struct Cli {
    /// Emit the versioned machine-readable report.
    #[arg(long)]
    json: bool,

    /// Require a capability such as write:flac, runtime:ffmpeg, or cpu:avx2.
    #[arg(long = "require", value_name = "CAPABILITY")]
    requirements: Vec<String>,
}

#[derive(Serialize)]
struct DoctorReport {
    schema_version: &'static str,
    generator: String,
    target: Target,
    parallelism: usize,
    features: Vec<FeatureCapability>,
    runtime: Vec<RuntimeCapability>,
    cpu: Vec<CpuCapability>,
    formats: Vec<FormatCapability>,
    requirements: Vec<RequirementResult>,
    ok: bool,
}

#[derive(Serialize)]
struct Target {
    os: &'static str,
    arch: &'static str,
    family: &'static str,
}

#[derive(Serialize)]
struct FeatureCapability {
    id: &'static str,
    compiled: bool,
}

#[derive(Serialize)]
struct RuntimeCapability {
    id: &'static str,
    available: bool,
    detail: String,
}

#[derive(Serialize)]
struct CpuCapability {
    id: &'static str,
    available: bool,
}

#[derive(Serialize)]
struct FormatCapability {
    format: &'static str,
    read: bool,
    write: bool,
    write_dependency: Option<&'static str>,
}

#[derive(Serialize)]
struct RequirementResult {
    id: String,
    available: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("forge-doctor: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode, String> {
    let features = feature_capabilities();
    let runtime = runtime_capabilities();
    let cpu = cpu_capabilities();
    let formats = format_capabilities(&runtime);
    let requirements = cli
        .requirements
        .iter()
        .map(|id| {
            capability_available(id, &features, &runtime, &cpu, &formats).map(|available| {
                RequirementResult {
                    id: id.clone(),
                    available,
                }
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let ok = requirements.iter().all(|item| item.available);
    let report = DoctorReport {
        schema_version: SCHEMA_VERSION,
        generator: format!("forge-normalizer/{}", env!("CARGO_PKG_VERSION")),
        target: Target {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            family: std::env::consts::FAMILY,
        },
        parallelism: thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1),
        features,
        runtime,
        cpu,
        formats,
        requirements,
        ok,
    };

    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|error| format!("serialize report: {error}"))?
        );
    } else {
        print_human(&report);
    }

    Ok(if report.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn feature_capabilities() -> Vec<FeatureCapability> {
    vec![
        feature("aac-encoding", cfg!(feature = "aac-encoding")),
        feature("clap-plugin", cfg!(feature = "clap-plugin")),
        feature("cuda-truepeak", cfg!(feature = "cuda-truepeak")),
        feature("ffmpeg-encoding", cfg!(feature = "ffmpeg-encoding")),
        feature("grpc-service", cfg!(feature = "grpc-service")),
        feature("lv2-plugin", cfg!(feature = "lv2-plugin")),
        feature("mp3-encoding", cfg!(feature = "mp3-encoding")),
        feature("onnx-provider", cfg!(feature = "onnx-provider")),
        feature("opus-encoding", cfg!(feature = "opus-encoding")),
    ]
}

fn feature(id: &'static str, compiled: bool) -> FeatureCapability {
    FeatureCapability { id, compiled }
}

fn runtime_capabilities() -> Vec<RuntimeCapability> {
    vec![probe_command("ffmpeg", &["-version"])]
}

fn probe_command(id: &'static str, arguments: &[&str]) -> RuntimeCapability {
    let mut child = match Command::new(id)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return RuntimeCapability {
                id,
                available: false,
                detail: "command not found on PATH".into(),
            };
        }
        Err(error) => {
            return RuntimeCapability {
                id,
                available: false,
                detail: format!("could not start command: {error}"),
            };
        }
    };

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return RuntimeCapability {
                    id,
                    available: status.success(),
                    detail: if status.success() {
                        "command completed successfully".into()
                    } else {
                        format!("command exited with {status}")
                    },
                };
            }
            Ok(None) if started.elapsed() < PROBE_TIMEOUT => {
                thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return RuntimeCapability {
                    id,
                    available: false,
                    detail: "command did not finish within 2 seconds".into(),
                };
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return RuntimeCapability {
                    id,
                    available: false,
                    detail: format!("could not query command status: {error}"),
                };
            }
        }
    }
}

fn cpu_capabilities() -> Vec<CpuCapability> {
    vec![
        cpu("avx2", has_avx2()),
        cpu("fma", has_fma()),
        cpu("sse4.1", has_sse41()),
        cpu("neon", has_neon()),
    ]
}

fn cpu(id: &'static str, available: bool) -> CpuCapability {
    CpuCapability { id, available }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn has_avx2() -> bool {
    std::arch::is_x86_feature_detected!("avx2")
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn has_avx2() -> bool {
    false
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn has_fma() -> bool {
    std::arch::is_x86_feature_detected!("fma")
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn has_fma() -> bool {
    false
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn has_sse41() -> bool {
    std::arch::is_x86_feature_detected!("sse4.1")
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn has_sse41() -> bool {
    false
}

#[cfg(target_arch = "aarch64")]
fn has_neon() -> bool {
    std::arch::is_aarch64_feature_detected!("neon")
}

#[cfg(target_arch = "arm")]
fn has_neon() -> bool {
    std::arch::is_arm_feature_detected!("neon")
}

#[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
fn has_neon() -> bool {
    false
}

fn format_capabilities(runtime: &[RuntimeCapability]) -> Vec<FormatCapability> {
    let ffmpeg = runtime
        .iter()
        .find(|item| item.id == "ffmpeg")
        .is_some_and(|item| item.available);
    let ffmpeg_write = cfg!(feature = "ffmpeg-encoding") && ffmpeg;
    vec![
        format("wav", true, true, None),
        format("flac", true, true, None),
        format(
            "mp3",
            true,
            cfg!(feature = "mp3-encoding"),
            Some("feature:mp3-encoding"),
        ),
        format(
            "opus",
            cfg!(feature = "opus-encoding"),
            cfg!(feature = "opus-encoding"),
            Some("feature:opus-encoding"),
        ),
        format(
            "aac",
            true,
            ffmpeg_write,
            Some("feature:ffmpeg-encoding + runtime:ffmpeg"),
        ),
        format(
            "m4a",
            true,
            ffmpeg_write,
            Some("feature:ffmpeg-encoding + runtime:ffmpeg"),
        ),
        format(
            "alac",
            true,
            ffmpeg_write,
            Some("feature:ffmpeg-encoding + runtime:ffmpeg"),
        ),
        format(
            "vorbis",
            true,
            ffmpeg_write,
            Some("feature:ffmpeg-encoding + runtime:ffmpeg"),
        ),
        format("dsf", true, false, Some("read-only")),
        format("dff", true, false, Some("read-only")),
    ]
}

fn format(
    format: &'static str,
    read: bool,
    write: bool,
    write_dependency: Option<&'static str>,
) -> FormatCapability {
    FormatCapability {
        format,
        read,
        write,
        write_dependency,
    }
}

fn capability_available(
    id: &str,
    features: &[FeatureCapability],
    runtime: &[RuntimeCapability],
    cpu: &[CpuCapability],
    formats: &[FormatCapability],
) -> Result<bool, String> {
    if let Some(name) = id.strip_prefix("feature:") {
        return features
            .iter()
            .find(|item| item.id == name)
            .map(|item| item.compiled)
            .ok_or_else(|| unknown_capability(id));
    }
    if let Some(name) = id.strip_prefix("runtime:") {
        return runtime
            .iter()
            .find(|item| item.id == name)
            .map(|item| item.available)
            .ok_or_else(|| unknown_capability(id));
    }
    if let Some(name) = id.strip_prefix("cpu:") {
        return cpu
            .iter()
            .find(|item| item.id == name)
            .map(|item| item.available)
            .ok_or_else(|| unknown_capability(id));
    }
    if let Some(name) = id.strip_prefix("read:") {
        return formats
            .iter()
            .find(|item| item.format == name)
            .map(|item| item.read)
            .ok_or_else(|| unknown_capability(id));
    }
    if let Some(name) = id.strip_prefix("write:") {
        return formats
            .iter()
            .find(|item| item.format == name)
            .map(|item| item.write)
            .ok_or_else(|| unknown_capability(id));
    }
    Err(unknown_capability(id))
}

fn unknown_capability(id: &str) -> String {
    format!(
        "unknown capability `{id}`; use feature:<name>, runtime:ffmpeg, cpu:<name>, read:<format>, or write:<format>"
    )
}

fn print_human(report: &DoctorReport) {
    println!(
        "Forge {} on {}/{} ({} worker(s))",
        env!("CARGO_PKG_VERSION"),
        report.target.os,
        report.target.arch,
        report.parallelism
    );
    println!("\nBuild features:");
    for item in &report.features {
        println!("  {:3} {}", yes_no(item.compiled), item.id);
    }
    println!("\nRuntime:");
    for item in &report.runtime {
        println!(
            "  {:3} {:<10} {}",
            yes_no(item.available),
            item.id,
            item.detail
        );
    }
    println!("\nCPU:");
    for item in &report.cpu {
        println!("  {:3} {}", yes_no(item.available), item.id);
    }
    println!("\nFormats:");
    println!("  {:<8} {:<5} {:<5} Dependency", "Format", "Read", "Write");
    for item in &report.formats {
        println!(
            "  {:<8} {:<5} {:<5} {}",
            item.format,
            yes_no(item.read),
            yes_no(item.write),
            item.write_dependency.unwrap_or("built in")
        );
    }
    if !report.requirements.is_empty() {
        println!("\nRequirements:");
        for item in &report.requirements {
            println!("  {:3} {}", yes_no(item.available), item.id);
        }
    }
    println!(
        "\nResult: {}",
        if report.ok {
            "READY"
        } else {
            "MISSING CAPABILITIES"
        }
    );
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}
