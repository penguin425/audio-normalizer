//! ATSC A/85:2026 Annex L multi-asset streaming-service loudness QC.
//!
//! This workflow measures decoded renders independently while retaining the
//! operator's codec and loudness-metadata declaration in a hashed request. It
//! does not claim to extract every proprietary codec's metadata natively.

use crate::normalization_diff::{self, FileEvidence, MeasurementEvidence};
use crate::normalize::{self, DialogueRange};
use crate::wav::named_channel_layout;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub const REQUEST_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/atsc-a85-service-request-v1";
pub const REPORT_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/atsc-a85-service-report-v1";
pub const STANDARD: &str = "ATSC A/85:2026-07";
pub const STANDARD_URL: &str = "https://www.atsc.org/atsc-documents/a85-techniques-for-establishing-and-maintaining-audio-loudness-for-digital-television/";

const MAX_REQUEST_BYTES: u64 = 1_048_576;
const MAX_ASSETS: usize = 64;
const MAX_DIALOGUE_RANGES_PER_ASSET: usize = 4_096;
const METADATA_TOLERANCE_LU: f64 = 2.0;
const DELIVERY_TOLERANCE_LU: f64 = 2.0;
const MAX_TRUE_PEAK_DBTP: f64 = -2.0;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceRequest {
    pub schema: String,
    pub service_id: String,
    #[serde(default = "default_target_lkfs")]
    pub target_lkfs: f64,
    #[serde(default)]
    pub target_authority: TargetAuthority,
    pub assets: Vec<AssetRequest>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetAuthority {
    #[default]
    RecommendedRange,
    PriorArrangement,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetRequest {
    pub id: String,
    pub path: PathBuf,
    pub programme_kind: ProgrammeKind,
    pub delivery_codec: DeliveryCodec,
    /// Human-readable origin of the codec/metadata declaration (packager,
    /// probe report, traffic system, or equivalent).
    pub declaration_source: String,
    pub declared_loudness_lkfs: Option<f64>,
    #[serde(default)]
    pub dialogue_free: bool,
    #[serde(default)]
    pub dialogue_ranges: Vec<DialogueRange>,
    pub accompanies: Option<String>,
    #[serde(default)]
    pub inserted: bool,
    pub channel_layout: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgrammeKind {
    LongForm,
    ShortForm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeliveryCodec {
    Ac3,
    Ac4,
    DtsUhd,
    EAc3,
    MpegH,
    XheAac,
    Aac,
    MpegLayer2,
    Mp3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataMode {
    Metadata,
    NonMetadata,
}

impl DeliveryCodec {
    pub const fn metadata_mode(self) -> MetadataMode {
        match self {
            Self::Ac3 | Self::Ac4 | Self::DtsUhd | Self::EAc3 | Self::MpegH | Self::XheAac => {
                MetadataMode::Metadata
            }
            Self::Aac | Self::MpegLayer2 | Self::Mp3 => MetadataMode::NonMetadata,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceReport {
    pub schema: &'static str,
    pub generator: &'static str,
    pub standard: StandardEvidence,
    pub method: MethodEvidence,
    pub request: FileEvidence,
    pub service_id: String,
    pub target_lkfs: f64,
    pub target_authority: TargetAuthority,
    pub assets: Vec<AssetEvidence>,
    pub service_checks: Vec<RuleCheck>,
    pub warnings: Vec<ServiceWarning>,
    pub warning_count: usize,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StandardEvidence {
    pub name: &'static str,
    pub approved: &'static str,
    pub source_url: &'static str,
    pub annex: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct MethodEvidence {
    pub id: &'static str,
    pub classification: &'static str,
    pub codec_metadata_evidence: &'static str,
    pub long_form_measurement: &'static str,
    pub short_form_measurement: &'static str,
    pub metadata_playback_estimate: &'static str,
    pub maximum_assets: usize,
    pub maximum_dialogue_ranges_per_asset: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssetEvidence {
    pub id: String,
    pub file: FileEvidence,
    pub programme_kind: ProgrammeKind,
    pub delivery_codec: DeliveryCodec,
    pub metadata_mode: MetadataMode,
    pub declaration_source: String,
    pub declared_loudness_lkfs: Option<f64>,
    pub programme: MeasurementEvidence,
    pub dialogue: Option<DialogueEvidence>,
    pub loudness_basis: &'static str,
    pub measured_loudness_lkfs: Option<f64>,
    pub metadata_deviation_lu: Option<f64>,
    pub normalized_playback_loudness_lkfs: Option<f64>,
    pub service_target_deviation_lu: Option<f64>,
    pub true_peak_headroom_db: Option<f64>,
    pub accompanies: Option<String>,
    pub inserted: bool,
    pub checks: Vec<RuleCheck>,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DialogueEvidence {
    pub integrated_lkfs: Option<f64>,
    pub duration_seconds: f64,
    pub range_count: usize,
    pub standard: &'static str,
    pub method: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuleCheck {
    pub rule_id: &'static str,
    pub clause: &'static str,
    pub passed: bool,
    pub message: String,
    pub observed: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceWarning {
    pub rule_id: &'static str,
    pub clause: &'static str,
    pub message: String,
    pub asset_ids: Vec<String>,
}

fn default_target_lkfs() -> f64 {
    -24.0
}

pub fn load_request(path: &Path) -> Result<ServiceRequest, String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "ATSC A/85 service request is not a file: {}",
            path.display()
        ));
    }
    if metadata.len() > MAX_REQUEST_BYTES {
        return Err(format!(
            "ATSC A/85 service request exceeds the {MAX_REQUEST_BYTES} byte limit"
        ));
    }
    let text =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let request = if path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("toml"))
    {
        toml::from_str(&text).map_err(|error| format!("parse {}: {error}", path.display()))?
    } else {
        serde_json::from_str(&text).map_err(|error| format!("parse {}: {error}", path.display()))?
    };
    validate_request(&request)?;
    Ok(request)
}

pub fn audit(request_path: &Path) -> Result<ServiceReport, String> {
    let request = load_request(request_path)?;
    let request_evidence = normalization_diff::inspect_file(request_path)?;
    let request_parent = request_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut warnings = Vec::new();
    let mut assets = Vec::with_capacity(request.assets.len());

    for requested in &request.assets {
        let path = if requested.path.is_absolute() {
            requested.path.clone()
        } else {
            request_parent.join(&requested.path)
        };
        let roles = requested
            .channel_layout
            .as_deref()
            .map(|name| {
                named_channel_layout(name).ok_or_else(|| {
                    format!("asset {} has unknown channel layout {name}", requested.id)
                })
            })
            .transpose()?;
        let programme = normalize::analyze_file_with_roles(&path, roles.as_deref())?;
        let programme_lkfs = finite(programme.lufs);
        let dialogue =
            if requested.programme_kind == ProgrammeKind::LongForm && !requested.dialogue_free {
                let measurement = normalize::analyze_dialogue_ranges_with_roles(
                    &path,
                    roles.as_deref(),
                    &requested.dialogue_ranges,
                )?;
                Some(DialogueEvidence {
                    integrated_lkfs: finite(measurement.lufs),
                    duration_seconds: measurement.duration_seconds,
                    range_count: measurement.range_count,
                    standard: measurement.standard,
                    method: measurement.method,
                })
            } else {
                None
            };
        if requested.dialogue_free {
            warnings.push(ServiceWarning {
                rule_id: "ATSC-A85-M1-DIALOGUE-FREE-FALLBACK",
                clause: "Annex M.1",
                message: format!(
                    "asset {} uses the explicitly declared rare dialogue-free full-programme fallback",
                    requested.id
                ),
                asset_ids: vec![requested.id.clone()],
            });
        }

        let (basis, measured_lkfs, basis_rule, basis_clause) = match requested.programme_kind {
            ProgrammeKind::LongForm if requested.dialogue_free => (
                "full_programme_explicit_dialogue_free",
                programme_lkfs,
                "ATSC-A85-L3-LONG-FORM-BASIS",
                "Annex L.3 / M.1",
            ),
            ProgrammeKind::LongForm => (
                "dialogue_anchor",
                dialogue.as_ref().and_then(|value| value.integrated_lkfs),
                "ATSC-A85-L3-LONG-FORM-BASIS",
                "Annex L.3",
            ),
            ProgrammeKind::ShortForm => (
                "full_programme",
                programme_lkfs,
                "ATSC-A85-L8-SHORT-FORM-BASIS",
                "Annex L.8",
            ),
        };
        let metadata_mode = requested.delivery_codec.metadata_mode();
        let metadata_deviation = match (metadata_mode, measured_lkfs) {
            (MetadataMode::Metadata, Some(measured)) => requested
                .declared_loudness_lkfs
                .map(|declared| measured - declared),
            _ => None,
        };
        let normalized_playback = match metadata_mode {
            MetadataMode::Metadata => {
                metadata_deviation.map(|deviation| request.target_lkfs + deviation)
            }
            MetadataMode::NonMetadata => measured_lkfs,
        };
        let target_deviation = normalized_playback.map(|value| value - request.target_lkfs);
        let true_peak = finite(programme.true_peak_db());
        let true_peak_headroom = true_peak.map(|value| MAX_TRUE_PEAK_DBTP - value);
        let mut checks = vec![RuleCheck {
            rule_id: basis_rule,
            clause: basis_clause,
            passed: measured_lkfs.is_some(),
            message: if measured_lkfs.is_some() {
                format!(
                    "asset {} has a finite {basis} loudness measurement",
                    requested.id
                )
            } else {
                format!(
                    "asset {} has no finite {basis} loudness measurement",
                    requested.id
                )
            },
            observed: json!({"basis": basis, "measured_lkfs": measured_lkfs}),
        }];

        match metadata_mode {
            MetadataMode::Metadata => {
                let passed =
                    metadata_deviation.is_some_and(|value| value.abs() <= METADATA_TOLERANCE_LU);
                checks.push(RuleCheck {
                    rule_id: "ATSC-A85-L4-METADATA-MATCH",
                    clause: "Annex L.4",
                    passed,
                    message: format!(
                        "asset {} codec loudness metadata {} the measured {} loudness within ±{METADATA_TOLERANCE_LU:.0} LU",
                        requested.id,
                        if passed { "matches" } else { "does not match" },
                        basis
                    ),
                    observed: json!({
                        "declared_loudness_lkfs": requested.declared_loudness_lkfs,
                        "measured_loudness_lkfs": measured_lkfs,
                        "deviation_lu": metadata_deviation,
                        "tolerance_lu": METADATA_TOLERANCE_LU,
                    }),
                });
            }
            MetadataMode::NonMetadata => {
                let passed =
                    target_deviation.is_some_and(|value| value.abs() <= DELIVERY_TOLERANCE_LU);
                checks.push(RuleCheck {
                    rule_id: "ATSC-A85-L5-NONMETADATA-TARGET",
                    clause: "Annex L.5",
                    passed,
                    message: format!(
                        "asset {} non-metadata loudness {} service target {:.2} LKFS within ±{DELIVERY_TOLERANCE_LU:.0} LU",
                        requested.id,
                        if passed { "matches" } else { "does not match" },
                        request.target_lkfs
                    ),
                    observed: json!({
                        "target_lkfs": request.target_lkfs,
                        "measured_loudness_lkfs": measured_lkfs,
                        "deviation_lu": target_deviation,
                        "tolerance_lu": DELIVERY_TOLERANCE_LU,
                    }),
                });
            }
        }
        if requested.inserted {
            let passed = target_deviation.is_some_and(|value| value.abs() <= DELIVERY_TOLERANCE_LU);
            checks.push(RuleCheck {
                rule_id: "ATSC-A85-L9-INSERTION-TARGET",
                clause: "Annex L.9",
                passed,
                message: format!(
                    "inserted asset {} normalized playback loudness {} service target within ±{DELIVERY_TOLERANCE_LU:.0} LU",
                    requested.id,
                    if passed { "matches" } else { "does not match" }
                ),
                observed: json!({
                    "target_lkfs": request.target_lkfs,
                    "normalized_playback_loudness_lkfs": normalized_playback,
                    "deviation_lu": target_deviation,
                    "tolerance_lu": DELIVERY_TOLERANCE_LU,
                }),
            });
        }
        let peak_passed = true_peak.is_some_and(|value| value <= MAX_TRUE_PEAK_DBTP);
        checks.push(RuleCheck {
            rule_id: "ATSC-A85-M-TRUE-PEAK",
            clause: "Annex M",
            passed: peak_passed,
            message: format!(
                "asset {} true peak {} the {MAX_TRUE_PEAK_DBTP:.0} dBTP maximum",
                requested.id,
                if peak_passed { "meets" } else { "exceeds" }
            ),
            observed: json!({
                "true_peak_dbtp": true_peak,
                "maximum_true_peak_dbtp": MAX_TRUE_PEAK_DBTP,
                "headroom_db": true_peak_headroom,
            }),
        });
        let passed = checks.iter().all(|check| check.passed);
        assets.push(AssetEvidence {
            id: requested.id.clone(),
            file: normalization_diff::inspect_file(&path)?,
            programme_kind: requested.programme_kind,
            delivery_codec: requested.delivery_codec,
            metadata_mode,
            declaration_source: requested.declaration_source.clone(),
            declared_loudness_lkfs: requested.declared_loudness_lkfs,
            programme: MeasurementEvidence::from(&programme),
            dialogue,
            loudness_basis: basis,
            measured_loudness_lkfs: measured_lkfs,
            metadata_deviation_lu: metadata_deviation,
            normalized_playback_loudness_lkfs: normalized_playback,
            service_target_deviation_lu: target_deviation,
            true_peak_headroom_db: true_peak_headroom,
            accompanies: requested.accompanies.clone(),
            inserted: requested.inserted,
            checks,
            passed,
        });
    }

    let by_id = assets
        .iter()
        .enumerate()
        .map(|(index, asset)| (asset.id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut service_checks = vec![RuleCheck {
        rule_id: "ATSC-A85-L5-SERVICE-TARGET",
        clause: "Annex L.5",
        passed: true,
        message: match request.target_authority {
            TargetAuthority::RecommendedRange => format!(
                "service target {:.2} LKFS is within the recommended -27 to -23 LKFS range",
                request.target_lkfs
            ),
            TargetAuthority::PriorArrangement => format!(
                "service target {:.2} LKFS is identified as a prior arrangement",
                request.target_lkfs
            ),
        },
        observed: json!({
            "target_lkfs": request.target_lkfs,
            "target_authority": request.target_authority,
            "recommended_minimum_lkfs": -27.0,
            "recommended_maximum_lkfs": -23.0,
        }),
    }];
    for short in assets
        .iter()
        .filter(|asset| asset.programme_kind == ProgrammeKind::ShortForm)
    {
        let long = &assets[*by_id
            .get(
                short
                    .accompanies
                    .as_deref()
                    .expect("validated short-form relationship"),
            )
            .expect("validated long-form relationship")];
        let passed = match (
            short.normalized_playback_loudness_lkfs,
            long.normalized_playback_loudness_lkfs,
        ) {
            (Some(short_loudness), Some(long_loudness)) => short_loudness <= long_loudness,
            _ => false,
        };
        service_checks.push(RuleCheck {
            rule_id: "ATSC-A85-L5-SHORT-NOT-LOUDER",
            clause: "Annex L.5",
            passed,
            message: format!(
                "short-form asset {} normalized playback loudness {} accompanying long-form asset {}",
                short.id,
                if passed { "does not exceed" } else { "exceeds or cannot be compared with" },
                long.id
            ),
            observed: json!({
                "short_asset_id": short.id,
                "short_normalized_playback_loudness_lkfs": short.normalized_playback_loudness_lkfs,
                "long_asset_id": long.id,
                "long_normalized_playback_loudness_lkfs": long.normalized_playback_loudness_lkfs,
            }),
        });
    }
    let metadata_assets = assets
        .iter()
        .filter(|asset| asset.metadata_mode == MetadataMode::Metadata)
        .count();
    let nonmetadata_assets = assets.len() - metadata_assets;
    let mixed_passed = assets.iter().all(|asset| {
        asset
            .checks
            .iter()
            .filter(|check| {
                matches!(
                    check.rule_id,
                    "ATSC-A85-L4-METADATA-MATCH" | "ATSC-A85-L5-NONMETADATA-TARGET"
                )
            })
            .all(|check| check.passed)
    });
    service_checks.push(RuleCheck {
        rule_id: "ATSC-A85-L6-L7-MIXED-MODE-CONSISTENCY",
        clause: "Annex L.6-L.7",
        passed: mixed_passed,
        message: if metadata_assets > 0 && nonmetadata_assets > 0 {
            format!(
                "mixed metadata/non-metadata service {} every constituent L.4/L.5 check",
                if mixed_passed { "passes" } else { "does not pass" }
            )
        } else {
            "service does not mix metadata and non-metadata assets; constituent checks remain authoritative".into()
        },
        observed: json!({
            "metadata_asset_count": metadata_assets,
            "nonmetadata_asset_count": nonmetadata_assets,
            "derived_only_from_constituent_l4_l5_checks": true,
        }),
    });

    let passed =
        assets.iter().all(|asset| asset.passed) && service_checks.iter().all(|check| check.passed);
    let warning_count = warnings.len();
    Ok(ServiceReport {
        schema: REPORT_SCHEMA,
        generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")),
        standard: StandardEvidence {
            name: STANDARD,
            approved: "2026-07-08",
            source_url: STANDARD_URL,
            annex: "Annex L with Annex M measurement and peak requirements",
        },
        method: MethodEvidence {
            id: "forge-atsc-a85-service-qc-v1",
            classification: "standards-backed decoded-render QC with request-declared delivery codec metadata",
            codec_metadata_evidence: "codec and encoded loudness values are operator declarations bound by the request SHA-256; decoded audio is measured independently",
            long_form_measurement: "explicit dialogue anchor using BS.1770-1 K-weighting without a relative-level gate; explicitly dialogue-free content uses the rare full-programme fallback",
            short_form_measurement: "complete programme over all non-LFE channels using ITU-R BS.1770-5 gating",
            metadata_playback_estimate: "service target plus measured-minus-declared loudness for metadata codecs; measured programme or anchor loudness for non-metadata codecs",
            maximum_assets: MAX_ASSETS,
            maximum_dialogue_ranges_per_asset: MAX_DIALOGUE_RANGES_PER_ASSET,
        },
        request: request_evidence,
        service_id: request.service_id,
        target_lkfs: request.target_lkfs,
        target_authority: request.target_authority,
        assets,
        service_checks,
        warnings,
        warning_count,
        passed,
    })
}

fn validate_request(request: &ServiceRequest) -> Result<(), String> {
    if request.schema != REQUEST_SCHEMA {
        return Err(format!(
            "unsupported ATSC A/85 service request schema: {}",
            request.schema
        ));
    }
    validate_identifier("service_id", &request.service_id)?;
    if !request.target_lkfs.is_finite() || !(-100.0..=0.0).contains(&request.target_lkfs) {
        return Err("target_lkfs must be finite and between -100 and 0".into());
    }
    if request.target_authority == TargetAuthority::RecommendedRange
        && !(-27.0..=-23.0).contains(&request.target_lkfs)
    {
        return Err(
            "recommended_range target_lkfs must be between -27 and -23; use prior_arrangement only when one exists"
                .into(),
        );
    }
    if request.assets.is_empty() || request.assets.len() > MAX_ASSETS {
        return Err(format!(
            "ATSC A/85 service request requires 1..={MAX_ASSETS} assets"
        ));
    }
    let mut ids = HashSet::new();
    for asset in &request.assets {
        validate_identifier("asset id", &asset.id)?;
        if !ids.insert(asset.id.clone()) {
            return Err(format!("duplicate asset id: {}", asset.id));
        }
        if asset.path.as_os_str().is_empty() || asset.path.to_string_lossy().len() > 4_096 {
            return Err(format!("asset {} has an empty or overlong path", asset.id));
        }
        if asset.declaration_source.trim().is_empty() || asset.declaration_source.len() > 512 {
            return Err(format!(
                "asset {} declaration_source must contain 1..=512 bytes",
                asset.id
            ));
        }
        if asset
            .declared_loudness_lkfs
            .is_some_and(|value| !value.is_finite() || !(-100.0..=0.0).contains(&value))
        {
            return Err(format!(
                "asset {} declared_loudness_lkfs must be finite and between -100 and 0",
                asset.id
            ));
        }
        match asset.delivery_codec.metadata_mode() {
            MetadataMode::Metadata if asset.declared_loudness_lkfs.is_none() => {
                return Err(format!(
                    "asset {} uses a metadata codec and requires declared_loudness_lkfs",
                    asset.id
                ));
            }
            MetadataMode::NonMetadata if asset.declared_loudness_lkfs.is_some() => {
                return Err(format!(
                    "asset {} uses a non-metadata codec and must not declare codec loudness metadata",
                    asset.id
                ));
            }
            _ => {}
        }
        if asset.dialogue_ranges.len() > MAX_DIALOGUE_RANGES_PER_ASSET {
            return Err(format!(
                "asset {} exceeds the {MAX_DIALOGUE_RANGES_PER_ASSET} dialogue-range limit",
                asset.id
            ));
        }
        match asset.programme_kind {
            ProgrammeKind::LongForm => {
                if asset.accompanies.is_some() {
                    return Err(format!(
                        "long-form asset {} must not accompany another asset",
                        asset.id
                    ));
                }
                if asset.dialogue_free {
                    if !asset.dialogue_ranges.is_empty() {
                        return Err(format!(
                            "dialogue-free asset {} must not define dialogue_ranges",
                            asset.id
                        ));
                    }
                } else {
                    normalize::validate_dialogue_ranges(&asset.dialogue_ranges)
                        .map_err(|error| format!("asset {}: {error}", asset.id))?;
                }
            }
            ProgrammeKind::ShortForm => {
                if asset.dialogue_free || !asset.dialogue_ranges.is_empty() {
                    return Err(format!(
                        "short-form asset {} uses full-programme measurement and must not define dialogue fields",
                        asset.id
                    ));
                }
                if asset.accompanies.is_none() {
                    return Err(format!(
                        "short-form asset {} requires an accompanying long-form asset id",
                        asset.id
                    ));
                }
            }
        }
        if asset.inserted && asset.programme_kind != ProgrammeKind::ShortForm {
            return Err(format!("inserted asset {} must be short-form", asset.id));
        }
        if let Some(layout) = &asset.channel_layout {
            if named_channel_layout(layout).is_none() {
                return Err(format!(
                    "asset {} has unknown channel layout {layout}",
                    asset.id
                ));
            }
        }
    }
    let by_id = request
        .assets
        .iter()
        .map(|asset| (asset.id.as_str(), asset))
        .collect::<HashMap<_, _>>();
    for asset in &request.assets {
        if let Some(long_id) = asset.accompanies.as_deref() {
            let long = by_id
                .get(long_id)
                .ok_or_else(|| format!("asset {} accompanies unknown asset {long_id}", asset.id))?;
            if long.programme_kind != ProgrammeKind::LongForm {
                return Err(format!(
                    "asset {} must accompany a long-form asset, but {long_id} is short-form",
                    asset.id
                ));
            }
        }
    }
    Ok(())
}

fn validate_identifier(field: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(format!(
            "{field} must contain 1..=64 ASCII letters, digits, '.', '-', or '_'"
        ));
    }
    Ok(())
}

fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(id: &str, kind: ProgrammeKind, codec: DeliveryCodec) -> AssetRequest {
        AssetRequest {
            id: id.into(),
            path: PathBuf::from(format!("{id}.wav")),
            programme_kind: kind,
            delivery_codec: codec,
            declaration_source: "unit-test declaration".into(),
            declared_loudness_lkfs: (codec.metadata_mode() == MetadataMode::Metadata)
                .then_some(-24.0),
            dialogue_free: kind == ProgrammeKind::LongForm,
            dialogue_ranges: Vec::new(),
            accompanies: (kind == ProgrammeKind::ShortForm).then(|| "long".into()),
            inserted: false,
            channel_layout: Some("stereo".into()),
        }
    }

    fn request(assets: Vec<AssetRequest>) -> ServiceRequest {
        ServiceRequest {
            schema: REQUEST_SCHEMA.into(),
            service_id: "service".into(),
            target_lkfs: -24.0,
            target_authority: TargetAuthority::RecommendedRange,
            assets,
        }
    }

    #[test]
    fn classifies_annex_l_metadata_codecs() {
        for codec in [
            DeliveryCodec::Ac3,
            DeliveryCodec::Ac4,
            DeliveryCodec::DtsUhd,
            DeliveryCodec::EAc3,
            DeliveryCodec::MpegH,
            DeliveryCodec::XheAac,
        ] {
            assert_eq!(codec.metadata_mode(), MetadataMode::Metadata);
        }
        for codec in [
            DeliveryCodec::Aac,
            DeliveryCodec::MpegLayer2,
            DeliveryCodec::Mp3,
        ] {
            assert_eq!(codec.metadata_mode(), MetadataMode::NonMetadata);
        }
    }

    #[test]
    fn validates_short_to_long_relationships() {
        let long = asset("long", ProgrammeKind::LongForm, DeliveryCodec::Ac3);
        let short = asset("short", ProgrammeKind::ShortForm, DeliveryCodec::Aac);
        assert!(validate_request(&request(vec![long, short])).is_ok());

        let mut invalid = asset("short", ProgrammeKind::ShortForm, DeliveryCodec::Aac);
        invalid.accompanies = Some("missing".into());
        assert!(validate_request(&request(vec![invalid])).is_err());
    }

    #[test]
    fn requires_codec_metadata_only_for_metadata_codecs() {
        let mut metadata = asset("long", ProgrammeKind::LongForm, DeliveryCodec::Ac3);
        metadata.declared_loudness_lkfs = None;
        assert!(validate_request(&request(vec![metadata])).is_err());

        let mut nonmetadata = asset("long", ProgrammeKind::LongForm, DeliveryCodec::Aac);
        nonmetadata.declared_loudness_lkfs = Some(-24.0);
        assert!(validate_request(&request(vec![nonmetadata])).is_err());
    }

    #[test]
    fn custom_targets_require_prior_arrangement() {
        let mut value = request(vec![asset(
            "long",
            ProgrammeKind::LongForm,
            DeliveryCodec::Ac3,
        )]);
        value.target_lkfs = -20.0;
        assert!(validate_request(&value).is_err());
        value.target_authority = TargetAuthority::PriorArrangement;
        assert!(validate_request(&value).is_ok());
    }
}
