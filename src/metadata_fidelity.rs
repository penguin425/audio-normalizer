//! Policy and audit primitives for loss-aware metadata handling.
//!
//! This module deliberately does not parse a particular container. Container
//! implementations provide MetadataLedgerEntry values for the fields they
//! discover, and this module gives every caller the same policy semantics,
//! deterministic ordering, and publication gate. Keeping the ledger
//! container-neutral is important: an unknown RIFF chunk, an ID3 frame, and an
//! ISO-BMFF atom must be reportable without first being forced through one
//! lossy tag abstraction.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

use crate::sample_time::{RoundingMode, SampleTimeTransform};

/// Published JSON contract for a metadata-fidelity report.
pub const METADATA_FIDELITY_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/metadata-fidelity-report-v1";
/// The schema version represented by MetadataFidelityReport.
pub const METADATA_FIDELITY_SCHEMA_VERSION: u32 = 1;
/// Revision of the container-independent field registry used by this API.
pub const METADATA_REGISTRY_REVISION: &str = "metadata-registry-v1";
/// Revision of the source/output sample-time evidence contract.
pub const METADATA_TIMING_REVISION: &str = "sample-time-transform-v1";

/// Maximum number of field decisions retained in one fidelity report.
///
/// The default registry can contribute two 100,000-entry region inventories,
/// one bounded timing ledger, and up to one aggregate issue marker per source
/// and destination.  One million leaves room for all of those decisions while
/// keeping report validation and publication resource-bounded.
pub const MAX_METADATA_FIDELITY_ENTRIES: usize = 1_000_000;
/// Maximum number of stable IDs retained in a report publication decision.
pub const MAX_METADATA_FIDELITY_BLOCKING_ENTRIES: usize = 1_000_000;

/// How a metadata writer treats fields it discovers.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MetadataPolicy {
    /// Keep fields whenever a representation is available and report losses.
    #[default]
    Preserve,
    /// Permit publication only when every field remains lossless and present.
    Strict,
    /// Permit removal only for the declared strip scope.
    Strip,
    /// Preserve the historical primary/first generic-tag behaviour explicitly.
    LegacyGeneric,
}

impl MetadataPolicy {
    pub const fn requires_lossless_publication(self) -> bool {
        matches!(self, Self::Strict)
    }

    pub const fn is_strip(self) -> bool {
        matches!(self, Self::Strip)
    }
}

/// Scope of an explicit MetadataPolicy::Strip request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StripScope {
    /// No stripping is configured. This is required for non-strip policies.
    None,
    /// Every discovered field may be dropped, subject to ledger reporting.
    All,
    /// Only the exact locators in strip_locators may be dropped.
    Selected,
}

/// Policy plus the explicit scope used when stripping metadata.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MetadataPolicyConfig {
    pub policy: MetadataPolicy,
    pub strip_scope: StripScope,
    /// Exact source metadata-inventory locators eligible for policy-driven
    /// removal; these are not filesystem paths or generic field names.
    pub strip_locators: Vec<String>,
}

impl Default for MetadataPolicyConfig {
    fn default() -> Self {
        Self::preserve()
    }
}

impl MetadataPolicyConfig {
    /// Construct the default best-effort preservation policy.
    pub fn preserve() -> Self {
        Self {
            policy: MetadataPolicy::Preserve,
            strip_scope: StripScope::None,
            strip_locators: Vec::new(),
        }
    }

    /// Construct the strict lossless policy.
    pub fn strict() -> Self {
        Self {
            policy: MetadataPolicy::Strict,
            strip_scope: StripScope::None,
            strip_locators: Vec::new(),
        }
    }

    /// Construct a policy that permits dropping all discovered fields.
    pub fn strip_all() -> Self {
        Self {
            policy: MetadataPolicy::Strip,
            strip_scope: StripScope::All,
            strip_locators: Vec::new(),
        }
    }

    /// Construct a policy that permits dropping only the listed locators.
    ///
    /// Locators are sorted so programmatically constructed policies have one
    /// canonical order. Duplicate locators are rejected by validate rather
    /// than silently changing the requested strip scope.
    pub fn strip_selected<I, S>(locators: I) -> Result<Self, MetadataFidelityError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut strip_locators = Vec::new();
        for locator in locators {
            if strip_locators.len() >= MAX_METADATA_FIDELITY_ENTRIES {
                return Err(MetadataFidelityError::InvalidPolicy(format!(
                    "strip_locators exceeds the configured limit {}",
                    MAX_METADATA_FIDELITY_ENTRIES
                )));
            }
            strip_locators.push(locator.into());
        }
        strip_locators.sort_unstable();
        let policy = Self {
            policy: MetadataPolicy::Strip,
            strip_scope: StripScope::Selected,
            strip_locators,
        };
        policy.validate()?;
        Ok(policy)
    }

    /// Construct the explicitly named compatibility policy.
    pub fn legacy_generic() -> Self {
        Self {
            policy: MetadataPolicy::LegacyGeneric,
            strip_scope: StripScope::None,
            strip_locators: Vec::new(),
        }
    }

    pub fn policy(&self) -> MetadataPolicy {
        self.policy
    }

    pub fn strip_scope(&self) -> StripScope {
        self.strip_scope
    }

    pub fn strip_locators(&self) -> &[String] {
        &self.strip_locators
    }

    pub fn requires_lossless_publication(&self) -> bool {
        self.policy.requires_lossless_publication()
    }

    pub fn is_strip(&self) -> bool {
        self.policy.is_strip()
    }

    /// Validate policy/scope combinations and locator ordering.
    pub fn validate(&self) -> Result<(), MetadataFidelityError> {
        if self.strip_locators.len() > MAX_METADATA_FIDELITY_ENTRIES {
            return Err(MetadataFidelityError::InvalidPolicy(format!(
                "strip_locators exceeds the configured limit {}",
                MAX_METADATA_FIDELITY_ENTRIES
            )));
        }
        match (self.policy, self.strip_scope) {
            (MetadataPolicy::Strip, StripScope::All | StripScope::Selected) => {}
            (MetadataPolicy::Strip, StripScope::None) => {
                return Err(MetadataFidelityError::InvalidPolicy(
                    "strip policy requires strip_scope=all or selected".into(),
                ));
            }
            (
                MetadataPolicy::Preserve | MetadataPolicy::Strict | MetadataPolicy::LegacyGeneric,
                StripScope::None,
            ) => {}
            (_, scope) => {
                return Err(MetadataFidelityError::InvalidPolicy(format!(
                    "strip_scope={scope:?} is only valid with policy=strip"
                )));
            }
        }
        match self.strip_scope {
            StripScope::None | StripScope::All if !self.strip_locators.is_empty() => {
                return Err(MetadataFidelityError::InvalidPolicy(
                    "strip_locators must be empty for this strip scope".into(),
                ));
            }
            StripScope::Selected if self.strip_locators.is_empty() => {
                return Err(MetadataFidelityError::InvalidPolicy(
                    "selected strip scope requires at least one locator".into(),
                ));
            }
            _ => {}
        }
        for locator in &self.strip_locators {
            validate_locator(locator).map_err(|reason| {
                MetadataFidelityError::InvalidPolicy(format!(
                    "invalid strip locator {locator:?}: {reason}"
                ))
            })?;
        }
        if self
            .strip_locators
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        {
            return Err(MetadataFidelityError::InvalidPolicy(
                "strip_locators must be unique and lexicographically sorted".into(),
            ));
        }
        Ok(())
    }

    fn allows_drop(&self, entry: &MetadataLedgerEntry) -> bool {
        if entry.outcome != MetadataOutcome::Dropped {
            return true;
        }
        match self.strip_scope {
            StripScope::All => true,
            StripScope::Selected => entry
                .source_locator
                .as_deref()
                .or(entry.destination_locator.as_deref())
                .is_some_and(|entry_locator| {
                    self.strip_locators
                        .binary_search_by(|locator| locator.as_str().cmp(entry_locator))
                        .is_ok()
                }),
            StripScope::None => false,
        }
    }
}

/// Outcome recorded for one discovered metadata field.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MetadataOutcome {
    Preserved,
    Mapped,
    Recomputed,
    Dropped,
}

/// Why a field is not byte/meaning identical after the operation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MetadataLossClass {
    /// Source and destination representation are equivalent for the contract.
    None,
    /// The representation changed, but the declared semantic value remains.
    Representation,
    /// Meaning or timing may have changed and requires review.
    Semantic,
    /// No supported mapping exists.
    Unsupported,
    /// The field was removed by policy.
    Policy,
}

/// One deterministic, field-level metadata decision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MetadataLedgerEntry {
    /// Stable registry field name, independent of a specific container syntax.
    pub field: String,
    pub outcome: MetadataOutcome,
    pub loss_class: MetadataLossClass,
    pub reason: String,
    /// Container-specific source-inventory locator, if a source value existed.
    pub source_locator: Option<String>,
    /// Container-specific destination locator, if a destination value exists.
    /// Dropped entries must leave this as None.
    pub destination_locator: Option<String>,
    /// SHA-256 of the source field bytes, when source bytes exist.
    pub before_sha256: Option<String>,
    /// SHA-256 of the destination field bytes, when destination bytes exist.
    pub after_sha256: Option<String>,
}

impl MetadataLedgerEntry {
    /// Build a field entry by hashing the bytes supplied by a container adapter.
    ///
    /// The adapter still chooses the semantic outcome and locators; this helper
    /// only centralizes the digest representation used by every report.
    #[allow(clippy::too_many_arguments)]
    pub fn from_bytes(
        field: impl Into<String>,
        outcome: MetadataOutcome,
        loss_class: MetadataLossClass,
        reason: impl Into<String>,
        source_locator: Option<String>,
        destination_locator: Option<String>,
        before: Option<&[u8]>,
        after: Option<&[u8]>,
    ) -> Result<Self, MetadataFidelityError> {
        let entry = Self::new(
            field,
            outcome,
            loss_class,
            reason,
            source_locator,
            destination_locator,
            before.map(sha256_hex),
            after.map(sha256_hex),
        );
        entry.validate()?;
        Ok(entry)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        field: impl Into<String>,
        outcome: MetadataOutcome,
        loss_class: MetadataLossClass,
        reason: impl Into<String>,
        source_locator: Option<String>,
        destination_locator: Option<String>,
        before_sha256: Option<String>,
        after_sha256: Option<String>,
    ) -> Self {
        Self {
            field: field.into(),
            outcome,
            loss_class,
            reason: reason.into(),
            source_locator,
            destination_locator,
            before_sha256,
            after_sha256,
        }
    }

    pub fn field(&self) -> &str {
        &self.field
    }

    pub fn outcome(&self) -> MetadataOutcome {
        self.outcome
    }

    pub fn loss_class(&self) -> MetadataLossClass {
        self.loss_class
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub fn source_locator(&self) -> Option<&str> {
        self.source_locator.as_deref()
    }

    pub fn destination_locator(&self) -> Option<&str> {
        self.destination_locator.as_deref()
    }

    pub fn before_sha256(&self) -> Option<&str> {
        self.before_sha256.as_deref()
    }

    pub fn after_sha256(&self) -> Option<&str> {
        self.after_sha256.as_deref()
    }

    /// Return the stable ordering key used in every report.
    pub fn ordering_key(&self) -> (&str, Option<&str>, Option<&str>) {
        (
            &self.field,
            self.source_locator.as_deref(),
            self.destination_locator.as_deref(),
        )
    }

    /// Return the stable human/machine-readable identity used by gate errors.
    pub fn stable_id(&self) -> String {
        serde_json::to_string(&(
            self.field.as_str(),
            self.source_locator.as_deref(),
            self.destination_locator.as_deref(),
        ))
        .expect("metadata entry identity contains only strings and optional strings")
    }

    /// Validate field shape and outcome/hash invariants.
    pub fn validate(&self) -> Result<(), MetadataFidelityError> {
        if !has_contract_text(self.field.as_str()) || self.field.chars().count() > 256 {
            return Err(MetadataFidelityError::InvalidEntry {
                field: self.field.clone(),
                reason: "field must contain 1..=256 non-whitespace Unicode scalar values".into(),
            });
        }
        if self.field.chars().any(is_contract_control) {
            return Err(MetadataFidelityError::InvalidEntry {
                field: self.field.clone(),
                reason: "field must not contain control characters".into(),
            });
        }
        if !has_contract_text(self.reason.as_str())
            || self.reason.chars().count() > 4096
            || self.reason.chars().any(is_contract_control)
        {
            return Err(MetadataFidelityError::InvalidEntry {
                field: self.field.clone(),
                reason:
                    "reason must contain 1..=4096 non-whitespace, non-control Unicode scalar values"
                        .into(),
            });
        }
        for (name, locator) in [
            ("source_locator", self.source_locator.as_deref()),
            ("destination_locator", self.destination_locator.as_deref()),
        ] {
            if let Some(locator) = locator {
                validate_locator(locator).map_err(|reason| {
                    MetadataFidelityError::InvalidEntry {
                        field: self.field.clone(),
                        reason: format!("{name} is invalid: {reason}"),
                    }
                })?;
            }
        }
        for (name, digest) in [
            ("before_sha256", self.before_sha256.as_deref()),
            ("after_sha256", self.after_sha256.as_deref()),
        ] {
            if let Some(digest) = digest {
                if !is_sha256(digest) {
                    return Err(MetadataFidelityError::InvalidEntry {
                        field: self.field.clone(),
                        reason: format!("{name} must be 64 lowercase hexadecimal characters"),
                    });
                }
            }
        }
        match self.outcome {
            MetadataOutcome::Preserved => {
                require_some(&self.source_locator, &self.field, "source_locator")?;
                require_some(
                    &self.destination_locator,
                    &self.field,
                    "destination_locator",
                )?;
                require_some(&self.before_sha256, &self.field, "before_sha256")?;
                require_some(&self.after_sha256, &self.field, "after_sha256")?;
                if self.before_sha256 != self.after_sha256 {
                    return Err(MetadataFidelityError::InvalidEntry {
                        field: self.field.clone(),
                        reason: "preserved fields must have identical before/after SHA-256".into(),
                    });
                }
                if self.loss_class != MetadataLossClass::None {
                    return Err(MetadataFidelityError::InvalidEntry {
                        field: self.field.clone(),
                        reason: "preserved fields must use loss_class=none".into(),
                    });
                }
            }
            MetadataOutcome::Mapped => {
                require_some(&self.source_locator, &self.field, "source_locator")?;
                require_some(
                    &self.destination_locator,
                    &self.field,
                    "destination_locator",
                )?;
                require_some(&self.before_sha256, &self.field, "before_sha256")?;
                require_some(&self.after_sha256, &self.field, "after_sha256")?;
                if matches!(
                    self.loss_class,
                    MetadataLossClass::Unsupported | MetadataLossClass::Policy
                ) {
                    return Err(MetadataFidelityError::InvalidEntry {
                        field: self.field.clone(),
                        reason: "mapped fields cannot use unsupported or policy loss".into(),
                    });
                }
            }
            MetadataOutcome::Recomputed => {
                require_some(
                    &self.destination_locator,
                    &self.field,
                    "destination_locator",
                )?;
                require_some(&self.after_sha256, &self.field, "after_sha256")?;
                if self.loss_class == MetadataLossClass::Policy {
                    return Err(MetadataFidelityError::InvalidEntry {
                        field: self.field.clone(),
                        reason: "recomputed fields cannot use policy loss".into(),
                    });
                }
            }
            MetadataOutcome::Dropped => {
                require_some(&self.source_locator, &self.field, "source_locator")?;
                require_some(&self.before_sha256, &self.field, "before_sha256")?;
                if self.destination_locator.is_some() {
                    return Err(MetadataFidelityError::InvalidEntry {
                        field: self.field.clone(),
                        reason: "dropped fields must not have destination_locator".into(),
                    });
                }
                if self.after_sha256.is_some() {
                    return Err(MetadataFidelityError::InvalidEntry {
                        field: self.field.clone(),
                        reason: "dropped fields must not have after_sha256".into(),
                    });
                }
                if self.loss_class == MetadataLossClass::None {
                    return Err(MetadataFidelityError::InvalidEntry {
                        field: self.field.clone(),
                        reason: "dropped fields must declare a non-none loss class".into(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// A validated, deterministically ordered field ledger.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct MetadataLedger {
    entries: Vec<MetadataLedgerEntry>,
}

impl MetadataLedger {
    /// Validate and sort entries by field and source/destination locator.
    pub fn new(mut entries: Vec<MetadataLedgerEntry>) -> Result<Self, MetadataFidelityError> {
        if entries.len() > MAX_METADATA_FIDELITY_ENTRIES {
            return Err(report_entry_limit_error(entries.len()));
        }
        for entry in &entries {
            entry.validate()?;
        }
        entries.sort_by(compare_entries);
        for pair in entries.windows(2) {
            if pair[0].ordering_key() == pair[1].ordering_key() {
                return Err(MetadataFidelityError::DuplicateField(pair[0].stable_id()));
            }
        }
        Ok(Self { entries })
    }

    pub fn from_entries(
        entries: impl IntoIterator<Item = MetadataLedgerEntry>,
    ) -> Result<Self, MetadataFidelityError> {
        Self::new(collect_entries(entries)?)
    }

    pub fn entries(&self) -> &[MetadataLedgerEntry] {
        &self.entries
    }

    pub fn into_entries(self) -> Vec<MetadataLedgerEntry> {
        self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Confirm that a deserialized ledger is already in canonical order.
    pub fn validate(&self) -> Result<(), MetadataFidelityError> {
        if self.entries.len() > MAX_METADATA_FIDELITY_ENTRIES {
            return Err(report_entry_limit_error(self.entries.len()));
        }
        let canonical = Self::new(self.entries.clone())?;
        if canonical.entries != self.entries {
            return Err(MetadataFidelityError::InvalidReport(
                "ledger entries are not in deterministic order".into(),
            ));
        }
        Ok(())
    }
}

fn compare_entries(left: &MetadataLedgerEntry, right: &MetadataLedgerEntry) -> Ordering {
    left.ordering_key().cmp(&right.ordering_key())
}

/// Revision evidence attached to every fidelity report.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MetadataFidelityEvidence {
    pub registry_revision: String,
    pub timing_revision: String,
    /// Exact transform context when sample-indexed fields were interpreted.
    #[serde(deserialize_with = "deserialize_required_option")]
    pub sample_time_transform: Option<MetadataSampleTimeEvidence>,
}

/// Machine-readable clock evidence shared by every mapped timing field.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MetadataSampleTimeEvidence {
    pub source_rate_hz: u32,
    pub output_rate_hz: u32,
    /// Decimal `i128` keeps the full signed domain portable across JSON tools.
    pub crop_origin_source_frames: String,
    pub rounding: RoundingMode,
}

impl<'de> Deserialize<'de> for MetadataSampleTimeEvidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireEvidence {
            source_rate_hz: u32,
            output_rate_hz: u32,
            crop_origin_source_frames: String,
            rounding: RoundingMode,
        }

        let wire = WireEvidence::deserialize(deserializer)?;
        let evidence = Self {
            source_rate_hz: wire.source_rate_hz,
            output_rate_hz: wire.output_rate_hz,
            crop_origin_source_frames: wire.crop_origin_source_frames,
            rounding: wire.rounding,
        };
        evidence.validate().map_err(serde::de::Error::custom)?;
        Ok(evidence)
    }
}

impl MetadataSampleTimeEvidence {
    pub fn new(
        source_rate_hz: u32,
        output_rate_hz: u32,
        crop_origin_source_frames: i128,
        rounding: RoundingMode,
    ) -> Result<Self, MetadataFidelityError> {
        SampleTimeTransform::new(source_rate_hz, output_rate_hz).map_err(|error| {
            MetadataFidelityError::InvalidReport(format!("invalid sample-time evidence: {error}"))
        })?;
        Ok(Self {
            source_rate_hz,
            output_rate_hz,
            crop_origin_source_frames: crop_origin_source_frames.to_string(),
            rounding,
        })
    }

    pub fn validate(&self) -> Result<(), MetadataFidelityError> {
        SampleTimeTransform::new(self.source_rate_hz, self.output_rate_hz).map_err(|error| {
            MetadataFidelityError::InvalidReport(format!("invalid sample-time evidence: {error}"))
        })?;
        let parsed = self
            .crop_origin_source_frames
            .parse::<i128>()
            .map_err(|_| {
                MetadataFidelityError::InvalidReport(
                    "sample-time crop origin must be a canonical decimal i128".into(),
                )
            })?;
        if parsed.to_string() != self.crop_origin_source_frames {
            return Err(MetadataFidelityError::InvalidReport(
                "sample-time crop origin must be a canonical decimal i128".into(),
            ));
        }
        Ok(())
    }

    pub const fn source_rate_hz(&self) -> u32 {
        self.source_rate_hz
    }

    pub const fn output_rate_hz(&self) -> u32 {
        self.output_rate_hz
    }

    pub fn crop_origin_source_frames(&self) -> i128 {
        self.crop_origin_source_frames
            .parse()
            .expect("validated sample-time evidence has a canonical i128")
    }

    pub const fn rounding(&self) -> RoundingMode {
        self.rounding
    }
}

impl Default for MetadataFidelityEvidence {
    fn default() -> Self {
        Self {
            registry_revision: METADATA_REGISTRY_REVISION.into(),
            timing_revision: METADATA_TIMING_REVISION.into(),
            sample_time_transform: None,
        }
    }
}

impl MetadataFidelityEvidence {
    pub fn new(registry_revision: impl Into<String>, timing_revision: impl Into<String>) -> Self {
        Self {
            registry_revision: registry_revision.into(),
            timing_revision: timing_revision.into(),
            sample_time_transform: None,
        }
    }

    pub fn with_sample_time_transform(mut self, evidence: MetadataSampleTimeEvidence) -> Self {
        self.sample_time_transform = Some(evidence);
        self
    }

    pub fn validate(&self) -> Result<(), MetadataFidelityError> {
        validate_revision("registry_revision", &self.registry_revision)?;
        validate_revision("timing_revision", &self.timing_revision)?;
        if let Some(evidence) = &self.sample_time_transform {
            evidence.validate()?;
        }
        Ok(())
    }

    pub fn registry_revision(&self) -> &str {
        &self.registry_revision
    }

    pub fn timing_revision(&self) -> &str {
        &self.timing_revision
    }

    pub fn sample_time_transform(&self) -> Option<&MetadataSampleTimeEvidence> {
        self.sample_time_transform.as_ref()
    }
}

/// Reusable evaluator for one policy/evidence context.
///
/// Container adapters can retain this value while discovering fields and call
/// evaluate once discovery is complete. It owns no parser or file state.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct MetadataFidelityEvaluator {
    policy: MetadataPolicyConfig,
    evidence: MetadataFidelityEvidence,
}

impl MetadataFidelityEvaluator {
    pub fn new(
        policy: MetadataPolicyConfig,
        evidence: MetadataFidelityEvidence,
    ) -> Result<Self, MetadataFidelityError> {
        policy.validate()?;
        evidence.validate()?;
        Ok(Self { policy, evidence })
    }

    pub fn policy(&self) -> &MetadataPolicyConfig {
        &self.policy
    }

    pub fn evidence(&self) -> &MetadataFidelityEvidence {
        &self.evidence
    }

    pub fn evaluate(
        &self,
        entries: impl IntoIterator<Item = MetadataLedgerEntry>,
    ) -> Result<MetadataFidelityReport, MetadataFidelityError> {
        MetadataFidelityReport::new(
            self.policy.clone(),
            self.evidence.clone(),
            collect_entries(entries)?,
        )
    }

    /// Evaluate and fail closed if the resulting report cannot be published.
    pub fn require_publication(
        &self,
        entries: impl IntoIterator<Item = MetadataLedgerEntry>,
    ) -> Result<MetadataFidelityReport, MetadataFidelityError> {
        let report = self.evaluate(entries)?;
        report.require_publication()?;
        Ok(report)
    }
}

/// Publication decision emitted with the ledger.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MetadataPublication {
    pub status: PublicationStatus,
    /// Stable entry IDs that prevent publication. Always sorted.
    pub blocking_entries: Vec<String>,
}

impl MetadataPublication {
    pub fn allowed(&self) -> bool {
        self.status == PublicationStatus::Allowed
    }

    pub fn status(&self) -> PublicationStatus {
        self.status
    }

    pub fn blocking_entries(&self) -> &[String] {
        &self.blocking_entries
    }
}

/// Result of the policy gate.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PublicationStatus {
    Allowed,
    Blocked,
}

/// Versioned field-level metadata fidelity report.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MetadataFidelityReport {
    pub schema: String,
    pub schema_version: u32,
    pub policy: MetadataPolicyConfig,
    pub evidence: MetadataFidelityEvidence,
    pub entries: Vec<MetadataLedgerEntry>,
    pub publication: MetadataPublication,
}

impl MetadataFidelityReport {
    /// Build a report from discovered fields and compute its publication gate.
    pub fn new(
        policy: MetadataPolicyConfig,
        evidence: MetadataFidelityEvidence,
        entries: Vec<MetadataLedgerEntry>,
    ) -> Result<Self, MetadataFidelityError> {
        if entries.len() > MAX_METADATA_FIDELITY_ENTRIES {
            return Err(report_entry_limit_error(entries.len()));
        }
        policy.validate()?;
        evidence.validate()?;
        let ledger = MetadataLedger::new(entries)?;
        let blocking_entries = blocking_entries(&policy, ledger.entries())?;
        let status = if blocking_entries.is_empty() {
            PublicationStatus::Allowed
        } else {
            PublicationStatus::Blocked
        };
        Ok(Self {
            schema: METADATA_FIDELITY_SCHEMA.into(),
            schema_version: METADATA_FIDELITY_SCHEMA_VERSION,
            policy,
            evidence,
            entries: ledger.into_entries(),
            publication: MetadataPublication {
                status,
                blocking_entries,
            },
        })
    }

    /// Alias for callers that use evaluator terminology.
    pub fn evaluate(
        policy: MetadataPolicyConfig,
        evidence: MetadataFidelityEvidence,
        entries: Vec<MetadataLedgerEntry>,
    ) -> Result<Self, MetadataFidelityError> {
        Self::new(policy, evidence, entries)
    }

    /// Validate a report received over a JSON/API boundary.
    pub fn validate(&self) -> Result<(), MetadataFidelityError> {
        if self.entries.len() > MAX_METADATA_FIDELITY_ENTRIES {
            return Err(report_entry_limit_error(self.entries.len()));
        }
        if self.publication.blocking_entries.len() > MAX_METADATA_FIDELITY_BLOCKING_ENTRIES {
            return Err(report_blocking_entry_limit_error(
                self.publication.blocking_entries.len(),
            ));
        }
        if self.schema != METADATA_FIDELITY_SCHEMA {
            return Err(MetadataFidelityError::InvalidReport(format!(
                "schema must be {METADATA_FIDELITY_SCHEMA}"
            )));
        }
        if self.schema_version != METADATA_FIDELITY_SCHEMA_VERSION {
            return Err(MetadataFidelityError::InvalidReport(format!(
                "unsupported schema_version {}",
                self.schema_version
            )));
        }
        self.policy.validate()?;
        self.evidence.validate()?;
        let ledger = MetadataLedger {
            entries: self.entries.clone(),
        };
        ledger.validate()?;
        let expected = blocking_entries(&self.policy, ledger.entries())?;
        let expected_status = if expected.is_empty() {
            PublicationStatus::Allowed
        } else {
            PublicationStatus::Blocked
        };
        if self.publication.status != expected_status
            || self.publication.blocking_entries != expected
        {
            return Err(MetadataFidelityError::InvalidReport(
                "publication decision does not match policy and ledger".into(),
            ));
        }
        Ok(())
    }

    pub fn publication_allowed(&self) -> bool {
        self.publication.allowed()
    }

    pub fn policy(&self) -> &MetadataPolicyConfig {
        &self.policy
    }

    pub fn evidence(&self) -> &MetadataFidelityEvidence {
        &self.evidence
    }

    pub fn entries(&self) -> &[MetadataLedgerEntry] {
        &self.entries
    }

    pub fn publication(&self) -> &MetadataPublication {
        &self.publication
    }

    /// Enforce the strict publication gate at the caller's commit boundary.
    pub fn require_publication(&self) -> Result<(), MetadataFidelityError> {
        self.validate()?;
        if self.publication_allowed() {
            Ok(())
        } else {
            Err(MetadataFidelityError::PublicationBlocked(
                self.publication.blocking_entries.clone(),
            ))
        }
    }
}

fn blocking_entries(
    policy: &MetadataPolicyConfig,
    entries: &[MetadataLedgerEntry],
) -> Result<Vec<String>, MetadataFidelityError> {
    let mut blockers = Vec::new();
    let mut dropped_locators = BTreeSet::new();
    for entry in entries {
        if entry.outcome == MetadataOutcome::Dropped {
            if let Some(locator) = entry
                .source_locator
                .as_deref()
                .or(entry.destination_locator.as_deref())
            {
                dropped_locators.insert(locator);
            }
        }
        if policy.requires_lossless_publication()
            && (entry.outcome == MetadataOutcome::Dropped
                || entry.loss_class != MetadataLossClass::None)
        {
            push_blocker(&mut blockers, entry.stable_id())?;
        }
        if policy.is_strip() && !policy.allows_drop(entry) {
            push_blocker(&mut blockers, entry.stable_id())?;
        }
        if policy.is_strip()
            && entry.outcome != MetadataOutcome::Dropped
            && entry.loss_class != MetadataLossClass::None
        {
            // Strip authorizes only the declared source removals. It must not
            // silently turn unrelated representation or semantic loss into a
            // side effect of that request.
            push_blocker(&mut blockers, entry.stable_id())?;
        }
        if policy.strip_scope == StripScope::All
            && entry.source_locator.is_some()
            && entry.outcome != MetadataOutcome::Dropped
        {
            push_blocker(&mut blockers, entry.stable_id())?;
        }
    }
    if policy.strip_scope == StripScope::Selected {
        for locator in &policy.strip_locators {
            if !dropped_locators.contains(locator.as_str()) {
                push_blocker(&mut blockers, format!("strip-scope-missing|{locator}"))?;
            }
        }
    }
    blockers.sort_unstable();
    blockers.dedup();
    Ok(blockers)
}

fn collect_entries(
    entries: impl IntoIterator<Item = MetadataLedgerEntry>,
) -> Result<Vec<MetadataLedgerEntry>, MetadataFidelityError> {
    let mut collected = Vec::new();
    for entry in entries {
        if collected.len() >= MAX_METADATA_FIDELITY_ENTRIES {
            return Err(report_entry_limit_error(collected.len() + 1));
        }
        collected.push(entry);
    }
    Ok(collected)
}

fn push_blocker(blockers: &mut Vec<String>, blocker: String) -> Result<(), MetadataFidelityError> {
    if blockers.len() >= MAX_METADATA_FIDELITY_BLOCKING_ENTRIES {
        return Err(report_blocking_entry_limit_error(blockers.len() + 1));
    }
    blockers.push(blocker);
    Ok(())
}

fn report_entry_limit_error(count: usize) -> MetadataFidelityError {
    MetadataFidelityError::InvalidReport(format!(
        "metadata fidelity report contains {count} entries; maximum is {MAX_METADATA_FIDELITY_ENTRIES}"
    ))
}

fn report_blocking_entry_limit_error(count: usize) -> MetadataFidelityError {
    MetadataFidelityError::InvalidReport(format!(
        "metadata fidelity publication contains {count} blocking entries; maximum is {MAX_METADATA_FIDELITY_BLOCKING_ENTRIES}"
    ))
}

fn require_some<T>(
    value: &Option<T>,
    field: &str,
    name: &str,
) -> Result<(), MetadataFidelityError> {
    if value.is_none() {
        return Err(MetadataFidelityError::InvalidEntry {
            field: field.into(),
            reason: format!("{name} is required for this outcome"),
        });
    }
    Ok(())
}

fn validate_locator(locator: &str) -> Result<(), &'static str> {
    if !has_contract_text(locator) || locator.chars().count() > 1024 {
        return Err("must contain 1..=1024 non-whitespace Unicode scalar values");
    }
    if locator.chars().any(is_contract_control) {
        return Err("must not contain control characters");
    }
    Ok(())
}

fn validate_revision(name: &str, value: &str) -> Result<(), MetadataFidelityError> {
    if !has_contract_text(value)
        || value.chars().count() > 128
        || value.chars().any(is_contract_control)
    {
        return Err(MetadataFidelityError::InvalidReport(format!(
            "{name} must contain 1..=128 non-control Unicode scalar values"
        )));
    }
    Ok(())
}

/// The report schema measures string length in Unicode scalar values, which
/// is the same unit returned by `str::chars().count()`.  Keep the whitespace
/// and control predicates explicit so the Rust boundary validator and the
/// published ECMA-262 regular expressions have one auditable contract.
fn has_contract_text(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .any(|character| !is_ecma_whitespace(character))
}

fn is_contract_control(character: char) -> bool {
    matches!(character, '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}')
}

fn is_ecma_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Return the canonical lowercase SHA-256 representation used by ledger fields.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Compare two complete container paths using the bounded registry and return
/// a conservative field-level fidelity report.
///
/// This is useful at a metadata-only transaction's readback boundary. A
/// changed representation is never promoted to lossless without a
/// format-specific transformer proving that mapping.
pub fn evaluate_paths(
    source: &Path,
    destination: &Path,
    policy: &MetadataPolicyConfig,
) -> Result<MetadataFidelityReport, MetadataFidelityError> {
    crate::metadata_pipeline::evaluate_paths(source, destination, policy)
        .map_err(MetadataFidelityError::InvalidReport)
}

/// Errors returned before a metadata publication can be committed.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MetadataFidelityError {
    InvalidPolicy(String),
    InvalidEntry { field: String, reason: String },
    DuplicateField(String),
    InvalidReport(String),
    PublicationBlocked(Vec<String>),
}

impl fmt::Display for MetadataFidelityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPolicy(reason) => write!(formatter, "invalid metadata policy: {reason}"),
            Self::InvalidEntry { field, reason } => {
                write!(formatter, "invalid metadata field {field:?}: {reason}")
            }
            Self::DuplicateField(field) => write!(formatter, "duplicate metadata field: {field}"),
            Self::InvalidReport(reason) => write!(formatter, "invalid metadata report: {reason}"),
            Self::PublicationBlocked(entries) => write!(
                formatter,
                "metadata publication blocked by {} field(s): {}",
                entries.len(),
                entries.join(", ")
            ),
        }
    }
}

impl std::error::Error for MetadataFidelityError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn hash(byte: char) -> String {
        std::iter::repeat_n(byte, 64).collect()
    }

    fn preserved(field: &str, locator: &str) -> MetadataLedgerEntry {
        MetadataLedgerEntry {
            field: field.into(),
            outcome: MetadataOutcome::Preserved,
            loss_class: MetadataLossClass::None,
            reason: "raw field copied byte-for-byte".into(),
            source_locator: Some(locator.into()),
            destination_locator: Some(locator.into()),
            before_sha256: Some(hash('a')),
            after_sha256: Some(hash('a')),
        }
    }

    fn mapped(field: &str, source: &str, destination: &str) -> MetadataLedgerEntry {
        MetadataLedgerEntry::new(
            field,
            MetadataOutcome::Mapped,
            MetadataLossClass::Semantic,
            "mapped with an explicit semantic adapter",
            Some(source.into()),
            Some(destination.into()),
            Some(hash('a')),
            Some(hash('b')),
        )
    }

    fn dropped(field: &str, locator: &str) -> MetadataLedgerEntry {
        MetadataLedgerEntry {
            field: field.into(),
            outcome: MetadataOutcome::Dropped,
            loss_class: MetadataLossClass::Policy,
            reason: "explicitly requested by strip scope".into(),
            source_locator: Some(locator.into()),
            destination_locator: None,
            before_sha256: Some(hash('b')),
            after_sha256: None,
        }
    }

    #[test]
    fn policy_validation_distinguishes_strip_scope() {
        assert!(MetadataPolicyConfig::strict().validate().is_ok());
        assert!(MetadataPolicyConfig::strip_all().validate().is_ok());
        let selected = MetadataPolicyConfig::strip_selected(["id3:TIT2", "riff:JUNK"]).unwrap();
        assert_eq!(
            selected.strip_locators,
            vec!["id3:TIT2".to_string(), "riff:JUNK".to_string()]
        );
        assert!(MetadataPolicyConfig::strip_selected(["riff:JUNK", "riff:JUNK"]).is_err());
        assert!(MetadataPolicyConfig {
            policy: MetadataPolicy::Strip,
            strip_scope: StripScope::Selected,
            strip_locators: Vec::new(),
        }
        .validate()
        .is_err());
    }

    #[test]
    fn ledger_is_sorted_and_strict_publication_is_blocked() {
        let report = MetadataFidelityReport::new(
            MetadataPolicyConfig::strict(),
            MetadataFidelityEvidence::default(),
            vec![preserved("z.field", "z"), dropped("a.field", "a")],
        )
        .unwrap();
        assert_eq!(report.entries[0].field, "a.field");
        assert!(!report.publication_allowed());
        assert!(matches!(
            report.require_publication(),
            Err(MetadataFidelityError::PublicationBlocked(_))
        ));
    }

    #[test]
    fn dropped_entries_must_not_claim_a_destination_locator() {
        let mut entry = dropped("riff.drop", "riff:DROP");
        entry.destination_locator = Some("riff:OUTPUT".into());
        assert!(entry.validate().is_err());
    }

    #[test]
    fn stable_ids_are_unambiguous_for_delimiter_containing_values() {
        let first = mapped("a|b", "c", "d");
        let second = mapped("a", "b", "c|d");
        assert_ne!(first.ordering_key(), second.ordering_key());
        assert_ne!(first.stable_id(), second.stable_id());

        let report = MetadataFidelityReport::new(
            MetadataPolicyConfig::strict(),
            MetadataFidelityEvidence::default(),
            vec![first, second],
        )
        .unwrap();
        assert_eq!(report.publication.blocking_entries.len(), 2);
    }

    #[test]
    fn sample_time_evidence_is_typed_and_canonical() {
        let evidence =
            MetadataSampleTimeEvidence::new(48_000, 44_100, -240, RoundingMode::HalfUp).unwrap();
        assert_eq!(evidence.crop_origin_source_frames(), -240);
        evidence.validate().unwrap();

        let mut invalid = evidence;
        invalid.crop_origin_source_frames = "-0240".into();
        assert!(invalid.validate().is_err());
        assert!(MetadataSampleTimeEvidence::new(0, 44_100, 0, RoundingMode::HalfUp).is_err());

        let invalid_decimal = serde_json::json!({
            "source_rate_hz": 48_000,
            "output_rate_hz": 44_100,
            "crop_origin_source_frames": "-0240",
            "rounding": "half-up",
        });
        assert!(serde_json::from_value::<MetadataSampleTimeEvidence>(invalid_decimal).is_err());
        let invalid_rate = serde_json::json!({
            "source_rate_hz": 0,
            "output_rate_hz": 44_100,
            "crop_origin_source_frames": "0",
            "rounding": "half-up",
        });
        assert!(serde_json::from_value::<MetadataSampleTimeEvidence>(invalid_rate).is_err());
    }

    #[test]
    fn selected_strip_requires_exactly_declared_drops() {
        let policy = MetadataPolicyConfig::strip_selected(["riff:JUNK"]).unwrap();
        let report = MetadataFidelityReport::new(
            policy,
            MetadataFidelityEvidence::default(),
            vec![dropped("riff.junk", "riff:JUNK")],
        )
        .unwrap();
        assert!(report.publication_allowed());
        report.require_publication().unwrap();

        let policy = MetadataPolicyConfig::strip_selected(["riff:JUNK"]).unwrap();
        let report = MetadataFidelityReport::new(
            policy,
            MetadataFidelityEvidence::default(),
            vec![dropped("riff.other", "riff:OTHER")],
        )
        .unwrap();
        assert!(!report.publication_allowed());
        assert!(report
            .publication
            .blocking_entries
            .iter()
            .any(|entry| entry.contains("strip-scope")));

        let policy = MetadataPolicyConfig::strip_selected(["riff:JUNK"]).unwrap();
        let report = MetadataFidelityReport::new(
            policy,
            MetadataFidelityEvidence::default(),
            vec![preserved("riff.junk", "riff:JUNK")],
        )
        .unwrap();
        assert!(!report.publication_allowed());
        assert!(report
            .publication
            .blocking_entries
            .iter()
            .any(|entry| entry.contains("strip-scope-missing")));

        let policy = MetadataPolicyConfig::strip_selected(["riff:JUNK"]).unwrap();
        let report = MetadataFidelityReport::new(
            policy,
            MetadataFidelityEvidence::default(),
            vec![
                dropped("riff.junk", "riff:JUNK"),
                mapped("riff.unselected", "riff:KEEP", "riff:KEEP"),
            ],
        )
        .unwrap();
        assert!(
            !report.publication_allowed(),
            "strip must not authorize semantic loss in an unselected field"
        );
    }

    #[test]
    fn strip_all_requires_every_source_field_to_be_dropped() {
        let report = MetadataFidelityReport::new(
            MetadataPolicyConfig::strip_all(),
            MetadataFidelityEvidence::default(),
            vec![preserved("riff.keep", "riff:KEEP")],
        )
        .unwrap();
        assert!(!report.publication_allowed());

        let report = MetadataFidelityReport::new(
            MetadataPolicyConfig::strip_all(),
            MetadataFidelityEvidence::default(),
            vec![dropped("riff.drop", "riff:DROP")],
        )
        .unwrap();
        assert!(report.publication_allowed());
    }

    #[test]
    fn report_round_trips_and_conforms_to_published_schema() {
        let report = MetadataFidelityReport::new(
            MetadataPolicyConfig::preserve(),
            MetadataFidelityEvidence::default(),
            vec![preserved("riff.bext", "RIFF.bext")],
        )
        .unwrap();
        let encoded = serde_json::to_value(&report).unwrap();
        let decoded: MetadataFidelityReport = serde_json::from_value(encoded.clone()).unwrap();
        decoded.validate().unwrap();
        assert_eq!(decoded, report);

        let schema: Value = serde_json::from_str(include_str!(
            "../schema/metadata-fidelity-report-v1.schema.json"
        ))
        .unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let errors: Vec<_> = validator
            .iter_errors(&encoded)
            .map(|error| error.to_string())
            .collect();
        assert!(errors.is_empty(), "schema violations: {errors:#?}");
        assert_eq!(
            schema["properties"]["entries"]["maxItems"],
            MAX_METADATA_FIDELITY_ENTRIES
        );
        assert_eq!(
            schema["$defs"]["policy"]["properties"]["strip_locators"]["maxItems"],
            MAX_METADATA_FIDELITY_ENTRIES
        );
        assert_eq!(
            schema["$defs"]["publication"]["properties"]["blocking_entries"]["maxItems"],
            MAX_METADATA_FIDELITY_BLOCKING_ENTRIES
        );
    }

    #[test]
    fn report_deserialization_rejects_missing_contract_fields_and_unknown_fields() {
        let report = MetadataFidelityReport::new(
            MetadataPolicyConfig::preserve(),
            MetadataFidelityEvidence::default(),
            vec![preserved("riff.bext", "RIFF.bext")],
        )
        .unwrap();
        let encoded = serde_json::to_value(&report).unwrap();

        let mut missing_locators = encoded.clone();
        missing_locators["policy"]
            .as_object_mut()
            .unwrap()
            .remove("strip_locators");
        assert!(serde_json::from_value::<MetadataFidelityReport>(missing_locators).is_err());

        let mut missing_transform = encoded.clone();
        missing_transform["evidence"]
            .as_object_mut()
            .unwrap()
            .remove("sample_time_transform");
        assert!(serde_json::from_value::<MetadataFidelityReport>(missing_transform).is_err());

        let mut unknown = encoded;
        unknown["publication"]
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), Value::Bool(true));
        assert!(serde_json::from_value::<MetadataFidelityReport>(unknown).is_err());
    }

    #[test]
    fn text_boundaries_use_unicode_scalar_values_and_match_schema() {
        let field = "é".repeat(256);
        let locator = "界".repeat(1024);
        let mut entry = preserved(&field, &locator);
        entry.reason = "理由".repeat(2048);
        entry.validate().unwrap();

        let evidence = MetadataFidelityEvidence::new("é".repeat(128), "界".repeat(128));
        evidence.validate().unwrap();
        let report = MetadataFidelityReport::new(
            MetadataPolicyConfig::preserve(),
            evidence,
            vec![entry.clone()],
        )
        .unwrap();
        let encoded = serde_json::to_value(&report).unwrap();
        let schema: Value = serde_json::from_str(include_str!(
            "../schema/metadata-fidelity-report-v1.schema.json"
        ))
        .unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(validator.is_valid(&encoded));

        let mut field_too_long = entry.clone();
        field_too_long.field.push('é');
        assert!(field_too_long.validate().is_err());
        let mut field_too_long_json = encoded.clone();
        field_too_long_json["entries"][0]["field"] = Value::String("é".repeat(257));
        assert!(!validator.is_valid(&field_too_long_json));

        let mut reason_too_long = entry.clone();
        reason_too_long.reason.push('理');
        assert!(reason_too_long.validate().is_err());
        let mut reason_too_long_json = encoded.clone();
        reason_too_long_json["entries"][0]["reason"] = Value::String("理由".repeat(2049));
        assert!(!validator.is_valid(&reason_too_long_json));

        let mut locator_too_long = entry.clone();
        locator_too_long.source_locator = Some(format!("{locator}é"));
        assert!(locator_too_long.validate().is_err());
        let mut locator_too_long_json = encoded.clone();
        locator_too_long_json["entries"][0]["source_locator"] =
            Value::String(format!("{locator}é"));
        assert!(!validator.is_valid(&locator_too_long_json));

        let revision_too_long =
            MetadataFidelityEvidence::new("é".repeat(129), "timing-revision-v1");
        assert!(revision_too_long.validate().is_err());
        let mut revision_too_long_json = encoded;
        revision_too_long_json["evidence"]["registry_revision"] = Value::String("é".repeat(129));
        assert!(!validator.is_valid(&revision_too_long_json));
    }

    #[test]
    fn text_control_ranges_match_schema_at_the_boundary() {
        let report = MetadataFidelityReport::new(
            MetadataPolicyConfig::preserve(),
            MetadataFidelityEvidence::default(),
            vec![preserved("riff.bext", "RIFF.bext")],
        )
        .unwrap();
        let encoded = serde_json::to_value(&report).unwrap();
        let schema: Value = serde_json::from_str(include_str!(
            "../schema/metadata-fidelity-report-v1.schema.json"
        ))
        .unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();

        for control in ['\u{001f}', '\u{007f}', '\u{0080}', '\u{009f}'] {
            let mut entry = preserved("riff.bext", "RIFF.bext");
            entry.field = format!("field{control}name");
            assert!(
                entry.validate().is_err(),
                "Rust accepted U+{:04X}",
                control as u32
            );

            let mut invalid = encoded.clone();
            invalid["entries"][0]["field"] = Value::String(format!("field{control}name"));
            assert!(
                !validator.is_valid(&invalid),
                "schema accepted U+{:04X}",
                control as u32
            );
        }

        for whitespace in ['\u{1680}', '\u{2003}', '\u{202f}', '\u{feff}'] {
            let mut entry = preserved("riff.bext", "RIFF.bext");
            entry.field = whitespace.to_string();
            assert!(entry.validate().is_err(), "Rust accepted ECMA whitespace");

            let mut invalid = encoded.clone();
            invalid["entries"][0]["field"] = Value::String(whitespace.to_string());
            assert!(
                !validator.is_valid(&invalid),
                "schema accepted ECMA whitespace"
            );
        }
    }

    #[test]
    fn evaluator_hashes_bytes_and_exposes_a_commit_gate() {
        let entry = MetadataLedgerEntry::from_bytes(
            "riff.bext",
            MetadataOutcome::Preserved,
            MetadataLossClass::None,
            "copied without mutation",
            Some("RIFF.bext".into()),
            Some("RIFF.bext".into()),
            Some(b"same bytes"),
            Some(b"same bytes"),
        )
        .unwrap();
        assert_eq!(
            entry.before_sha256(),
            Some("58100dc8fc06562ce3e578231dc948e083520ee49c4b4ee5a5a28bb4b4003feb")
        );
        let evaluator =
            MetadataFidelityEvaluator::new(MetadataPolicyConfig::strict(), Default::default())
                .unwrap();
        let report = evaluator.require_publication([entry]).unwrap();
        assert!(report.publication_allowed());
    }
}
