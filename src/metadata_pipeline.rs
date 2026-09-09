//! Internal bridge between bounded container discovery and normalization.
//!
//! Public policy/report vocabulary lives in `metadata_fidelity`, while the
//! parser inventory is exposed by `metadata_registry`.  This module keeps the
//! writer-specific planning details private so the established `Plan` shape
//! and legacy normalization entry points remain source compatible.

use crate::metadata_fidelity::{
    sha256_hex, MetadataFidelityEvidence, MetadataFidelityReport, MetadataLedgerEntry,
    MetadataLossClass, MetadataOutcome, MetadataPolicy, MetadataPolicyConfig,
    MetadataSampleTimeEvidence, StripScope,
};
use crate::metadata_registry::{
    discover_path, ContainerKind, DiscoveryLimits, MetadataInventory, MetadataKind, MetadataRegion,
};
use crate::metadata_transaction::MetadataStage;
use crate::sample_time::RoundingMode;
use crate::wav::WaveChunk;
use crate::wave_metadata_timing::{
    transform_wave, TimingLedgerAction, TimingLedgerEntry, WaveTimingTransformOptions,
};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MetadataDestination {
    Wave,
    Flac,
    Mp3,
    OggOpus,
    IsoBmff,
    OggVorbis,
}

/// Policy plan captured before any destination stage is created.
pub(crate) struct PreparedMetadata {
    policy: MetadataPolicyConfig,
    evidence: MetadataFidelityEvidence,
    source: MetadataInventory,
    destination: MetadataDestination,
    wave_chunks: Option<Vec<WaveChunk>>,
    wave_facts: Vec<WaveFact>,
    expected_wave_by_source: HashMap<String, Vec<u8>>,
    expected_generated_bwf: Option<Vec<u8>>,
    selected: BTreeSet<String>,
    copy_generic: bool,
    plan_bwf: bool,
    clock_changed: bool,
    source_range_complete: bool,
}

/// Exact evidence captured from the authoritative output-adapter preimage and
/// immediately after its loudness writers complete. A destination-only entry
/// may be promoted to lossless `recomputed` only when all three identity
/// components match: the registry kind, the complete destination locator
/// (including ordinal), and the bytes' SHA-256 digest. Keeping this evidence
/// private prevents a public kind allow-list from blessing arbitrary injected
/// metadata.
#[derive(Clone, Debug)]
pub(crate) struct GeneratedDestinationRegion {
    kind: MetadataKind,
    locator: String,
    raw_sha256: String,
    origin: GeneratedDestinationOrigin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GeneratedDestinationOrigin {
    OutputAdapter,
    LoudnessWriter,
    ContainerAncestor,
}

/// The narrow set of destination writers that may introduce loudness
/// metadata during explicit normalization.  A writer scope is deliberately
/// separate from `MetadataKind`: the latter describes what was discovered,
/// while this enum records which adapter was actually authorized to produce
/// the region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DestinationLoudnessWriter {
    ReplayGain,
    OpusR128,
    IsoBmffNative,
    /// Apple Sound Check's `----:com.apple.iTunes:iTunNORM` freeform item.
    IsoBmffSoundCheck,
}

/// Result of the synchronous metadata-only loudness adapter.
///
/// The result intentionally exposes only the scheme, native-write status, and
/// validated report.  The preimage and writer-specific evidence remain inside
/// the library, so callers cannot manufacture a destination-only proof by
/// supplying an arbitrary kind/path/hash tuple.
#[derive(Clone, Debug)]
pub struct LoudnessMetadataWriteResult {
    scheme: crate::metadata::LoudnessMetadataScheme,
    isobmff_written: bool,
    sound_check: Option<crate::metadata::SoundCheck>,
    report: MetadataFidelityReport,
}

impl LoudnessMetadataWriteResult {
    pub fn scheme(&self) -> crate::metadata::LoudnessMetadataScheme {
        self.scheme
    }

    pub fn isobmff_written(&self) -> bool {
        self.isobmff_written
    }

    pub fn sound_check(&self) -> Option<&crate::metadata::SoundCheck> {
        self.sound_check.as_ref()
    }

    pub fn report(&self) -> &MetadataFidelityReport {
        &self.report
    }
}

/// Write the authoritative loudness metadata set and evaluate its fidelity in
/// one synchronous, library-owned preimage/postimage transaction.
///
/// This is the metadata-only path's narrow adapter API.  The destination
/// inventory is captured immediately before the existing loudness writers
/// run; exact writer-owned evidence is derived privately from that preimage
/// and the completed output, then included in the returned report.  Other
/// destination-only regions remain unsupported unless a future adapter adds
/// an equivalent writer-specific proof.
///
/// This function may run multiple in-place metadata writers. Its stage
/// capability is issued only inside [`MetadataTransaction::stage`](crate::metadata_transaction::MetadataTransaction::stage),
/// so it cannot be called directly on a live source pathname. The captured
/// stage preimage is the report's source inventory, and the live source
/// pathname is never reopened during mutation.
pub fn write_loudness_metadata_with_fidelity(
    stage: &MetadataStage,
    analysis: &crate::normalize::Analysis,
    album: Option<(f64, f32)>,
    isobmff_album: Option<(f64, f32, f32)>,
    sound_check: Option<&crate::metadata::SoundCheck>,
    policy: &MetadataPolicyConfig,
) -> Result<LoudnessMetadataWriteResult, String> {
    let destination_path = stage.path();
    policy.validate().map_err(|error| error.to_string())?;
    let before = discover_path(destination_path, DiscoveryLimits::default())
        .map_err(|error| format!("discover metadata write preimage: {error}"))?;
    let destination_kind = destination_for_container(before.container)?;
    let requested_sound_check = sound_check;
    let scheme = crate::metadata::write_loudness_metadata(
        destination_path,
        analysis.lufs,
        analysis.true_peak,
        album,
    )?;
    let isobmff_written = if before.container == ContainerKind::IsoBmff {
        crate::metadata::write_isobmff_loudness_metadata(destination_path, analysis, isobmff_album)?
    } else {
        false
    };
    // Lofty's M4A ReplayGain save can rebuild the freeform `ilst` and discard
    // an iTunNORM atom that was written earlier.  Sound Check is therefore
    // the final metadata writer, followed by the exact read-back below.
    let sound_check = requested_sound_check
        .map(|value| crate::metadata::write_sound_check(destination_path, value))
        .transpose()?;
    let destination = discover_path(destination_path, DiscoveryLimits::default())
        .map_err(|error| format!("discover output metadata: {error}"))?;
    if destination.container != before.container {
        return Err("metadata output container changed while writing loudness metadata".into());
    }
    let sound_check = requested_sound_check
        .map(|expected| {
            let actual = crate::metadata::read_sound_check(destination_path)?.ok_or_else(|| {
                "Sound Check metadata disappeared after the complete loudness writer set"
                    .to_string()
            })?;
            if &actual != expected {
                return Err(String::from(
                    "Sound Check metadata changed after the complete loudness writer set",
                ));
            }
            Ok(actual)
        })
        .transpose()?
        .or(sound_check);
    // The writer preimage is the exact private stage copied from the stable
    // transaction source. Use it as the source inventory instead of opening
    // the live source pathname again while the mutation is in flight.
    let prepared = prepared_for_evaluation(policy, before.clone(), destination_kind)?;
    let writers =
        loudness_writers_for_destination(destination_kind, requested_sound_check.is_some());
    let generated = prepared.destination_loudness_evidence(&before, &destination, &writers);
    let entries = prepared.ledger(&destination, None, &generated)?;
    let report =
        MetadataFidelityReport::new(policy.clone(), MetadataFidelityEvidence::default(), entries)
            .map_err(|error| error.to_string())?;
    Ok(LoudnessMetadataWriteResult {
        scheme,
        isobmff_written,
        sound_check,
        report,
    })
}

#[derive(Clone, Debug)]
struct WaveFact {
    source_locator: String,
    entry: TimingLedgerEntry,
}

pub(crate) fn evaluate_paths(
    source_path: &Path,
    destination_path: &Path,
    policy: &MetadataPolicyConfig,
) -> Result<MetadataFidelityReport, String> {
    policy.validate().map_err(|error| error.to_string())?;
    let source = discover_path(source_path, DiscoveryLimits::default())
        .map_err(|error| format!("discover source metadata: {error}"))?;
    let destination = discover_path(destination_path, DiscoveryLimits::default())
        .map_err(|error| format!("discover output metadata: {error}"))?;
    evaluate_inventories(policy, source, destination, &[])
}

fn evaluate_inventories(
    policy: &MetadataPolicyConfig,
    source: MetadataInventory,
    destination: MetadataInventory,
    generated_destination_regions: &[GeneratedDestinationRegion],
) -> Result<MetadataFidelityReport, String> {
    let destination_kind = destination_for_container(destination.container)?;
    let prepared = prepared_for_evaluation(policy, source, destination_kind)?;
    MetadataFidelityReport::new(
        policy.clone(),
        MetadataFidelityEvidence::default(),
        prepared.ledger(&destination, None, generated_destination_regions)?,
    )
    .map_err(|error| error.to_string())
}

fn prepared_for_evaluation(
    policy: &MetadataPolicyConfig,
    source: MetadataInventory,
    destination_kind: MetadataDestination,
) -> Result<PreparedMetadata, String> {
    policy.validate().map_err(|error| error.to_string())?;
    Ok(PreparedMetadata {
        policy: policy.clone(),
        evidence: MetadataFidelityEvidence::default(),
        source,
        destination: destination_kind,
        wave_chunks: None,
        wave_facts: Vec::new(),
        expected_wave_by_source: HashMap::new(),
        expected_generated_bwf: None,
        selected: policy.strip_locators().iter().cloned().collect(),
        copy_generic: false,
        plan_bwf: false,
        clock_changed: false,
        source_range_complete: true,
    })
}

impl PreparedMetadata {
    /// Discover and validate a source before output creation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare(
        source: &Path,
        destination: MetadataDestination,
        source_rate_hz: u32,
        output_rate_hz: u32,
        crop_origin_source_frames: i128,
        source_range_complete: bool,
        plan_bwf: bool,
        policy: &MetadataPolicyConfig,
    ) -> Result<Self, String> {
        policy.validate().map_err(|error| error.to_string())?;
        if policy.policy() == MetadataPolicy::LegacyGeneric {
            return Err(
                "legacy-generic metadata handling must use the compatibility normalization path"
                    .into(),
            );
        }
        let source = discover_path(source, DiscoveryLimits::default())
            .map_err(|error| format!("discover source metadata: {error}"))?;
        let selected = policy
            .strip_locators()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if selected.len() != policy.strip_locators().len() {
            return Err("metadata strip locators must be unique".into());
        }
        let source_locators = source
            .regions
            .iter()
            .map(|region| region_locator(source.container, region))
            .collect::<BTreeSet<_>>();
        if let Some(locator) = selected
            .iter()
            .find(|locator| !source_locators.contains(*locator))
        {
            return Err(format!(
                "metadata strip locator was not discovered in the source: {locator}"
            ));
        }

        let is_strip_all =
            policy.policy() == MetadataPolicy::Strip && policy.strip_scope() == StripScope::All;
        let is_strip_selected = policy.policy() == MetadataPolicy::Strip
            && policy.strip_scope() == StripScope::Selected;
        if is_strip_selected
            && !(source.container == ContainerKind::Wave
                && destination == MetadataDestination::Wave)
        {
            return Err(
                "selected metadata stripping is currently supported only for WAVE-to-WAVE normalization"
                    .into(),
            );
        }
        if policy.policy() == MetadataPolicy::Strict && !source.issues.is_empty() {
            return Err(format!(
                "strict metadata preflight found {} malformed or budget-limited source region(s): {}",
                source.issues.len(),
                source
                    .issues
                    .iter()
                    .map(|issue| issue.code.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if policy.policy() == MetadataPolicy::Strict
            && !source.regions.is_empty()
            && !(source.container == ContainerKind::Wave
                && destination == MetadataDestination::Wave)
        {
            return Err(format!(
                "strict metadata publication has no lossless adapter from {:?} to {:?}",
                source.container, destination
            ));
        }
        if policy.policy() == MetadataPolicy::Strict
            && source.container == ContainerKind::Wave
            && !source.regions.is_empty()
            && !source_range_complete
        {
            return Err(
                "strict metadata publication cannot prove that sample-indexed fields remain inside a truncated source range"
                .into(),
            );
        }
        if policy.policy() == MetadataPolicy::Strict && source.container == ContainerKind::Wave {
            if let Some(region) = source
                .regions
                .iter()
                .find(|region| !is_supported_wave_region(region))
            {
                return Err(format!(
                    "strict metadata preflight has no semantic adapter for {}",
                    region_locator(source.container, region)
                ));
            }
            if (source_rate_hz != output_rate_hz || crop_origin_source_frames != 0)
                && source.regions.iter().any(is_opaque_wave_timing_region)
            {
                return Err(
                    "strict metadata preflight cannot prove sample timing in opaque WAVE XML metadata"
                        .into(),
                );
            }
        }

        let mut wave_chunks = None;
        let mut wave_facts = Vec::new();
        let mut expected_wave_by_source = HashMap::new();
        let mut expected_generated_bwf = None;
        let evidence = if source.container == ContainerKind::Wave
            && destination == MetadataDestination::Wave
        {
            MetadataFidelityEvidence::default().with_sample_time_transform(
                MetadataSampleTimeEvidence::new(
                    source_rate_hz,
                    output_rate_hz,
                    crop_origin_source_frames,
                    RoundingMode::HalfUp,
                )
                .map_err(|error| error.to_string())?,
            )
        } else {
            MetadataFidelityEvidence::default()
        };
        if destination == MetadataDestination::Wave {
            let mut encoded = Vec::new();
            let mut included = Vec::new();
            if source.container == ContainerKind::Wave && !is_strip_all {
                for region in &source.regions {
                    let locator = region_locator(source.container, region);
                    if selected.contains(&locator) {
                        continue;
                    }
                    let MetadataKind::WaveChunk { .. } = &region.kind else {
                        continue;
                    };
                    let Some(raw) = region.raw.as_deref() else {
                        if policy.policy() == MetadataPolicy::Strict {
                            return Err(format!(
                                "strict metadata preflight could not retain source region {locator} within the configured bounds"
                            ));
                        }
                        continue;
                    };
                    validate_encoded_wave_chunk(raw, &locator)?;
                    encoded.push(raw.to_vec());
                    included.push(locator);
                }
            }
            let mut writer_chunks = Vec::new();
            if !encoded.is_empty() {
                let synthetic = synthetic_wave(&encoded)?;
                let timing_policy = match policy.policy() {
                    MetadataPolicy::Strict => MetadataPolicy::Strict,
                    MetadataPolicy::Strip => MetadataPolicy::Strip,
                    MetadataPolicy::Preserve => MetadataPolicy::Preserve,
                    MetadataPolicy::LegacyGeneric => unreachable!(
                        "legacy-generic metadata uses the compatibility normalization path"
                    ),
                };
                let timing = transform_wave(
                    &synthetic,
                    &WaveTimingTransformOptions::new(
                        source_rate_hz,
                        output_rate_hz,
                        crop_origin_source_frames,
                        timing_policy,
                        Vec::new(),
                    )
                    .map_err(|error| format!("configure WAVE metadata timing: {error}"))?,
                )
                .map_err(|error| format!("transform WAVE metadata timing: {error}"))?;
                if policy.policy() == MetadataPolicy::Strict
                    && (source_rate_hz != output_rate_hz || crop_origin_source_frames != 0)
                    && !timing.unsupported_xml_chunks.is_empty()
                {
                    return Err(format!(
                        "strict metadata preflight cannot prove sample timing in opaque WAVE XML chunk(s): {}",
                        timing
                            .unsupported_xml_chunks
                            .iter()
                            .map(|id| String::from_utf8_lossy(id).into_owned())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                for mut entry in timing.ledger {
                    if !source_range_complete
                        && matches!(entry.chunk_id, id if id == *b"cue " || id == *b"LIST" || id == *b"smpl")
                    {
                        // A range descriptor exposes its start but this layer
                        // deliberately does not guess the retained end from a
                        // partially decoded stream. Values that map cleanly
                        // can still point beyond that end, so Preserve must
                        // retain an explicit semantic-review marker.
                        entry.candidate = true;
                        entry.detail.push_str(
                            "; retained source range is incomplete, so field containment cannot be proven",
                        );
                    }
                    let source_locator = included
                        .get(entry.chunk_index)
                        .ok_or("WAVE timing ledger references an unknown metadata chunk")?
                        .clone();
                    wave_facts.push(WaveFact {
                        source_locator,
                        entry,
                    });
                }
                for raw in encoded_wave_chunks(&timing.bytes)? {
                    writer_chunks.push(WaveChunk {
                        id: raw[..4].try_into().unwrap(),
                        body: encoded_wave_chunk_body(raw)?.to_vec(),
                    });
                }
            }
            if plan_bwf {
                if let Some(bext) = writer_chunks.iter_mut().find(|chunk| chunk.id == *b"bext") {
                    // The loudness finalizer writes the BWF v2 version and
                    // five fixed-width loudness fields. A valid legacy bext
                    // can be shorter than those offsets, so make the writer
                    // representation large enough before it reaches disk.
                    bext.body.resize(bext.body.len().max(602), 0);
                    let version = u16::from_le_bytes([bext.body[346], bext.body[347]]).max(2);
                    bext.body[346..348].copy_from_slice(&version.to_le_bytes());
                } else {
                    writer_chunks.push(WaveChunk {
                        id: *b"bext",
                        body: crate::metadata::blank_bext(),
                    });
                }
            }
            if writer_chunks.len() < included.len() {
                return Err(
                    "WAVE timing adapter returned fewer chunks than the source metadata plan"
                        .into(),
                );
            }
            for (locator, chunk) in included.iter().zip(&writer_chunks) {
                expected_wave_by_source.insert(locator.clone(), encode_wave_chunk(chunk)?);
            }
            if writer_chunks.len() > included.len() {
                if writer_chunks.len() != included.len() + 1 || !plan_bwf {
                    return Err(
                        "WAVE metadata plan generated an unexpected destination-only chunk".into(),
                    );
                }
                expected_generated_bwf = Some(encode_wave_chunk(
                    writer_chunks
                        .last()
                        .expect("one generated WAVE chunk was counted"),
                )?);
            }
            wave_chunks = Some(writer_chunks);
        }

        let copy_generic = !is_strip_all
            && !is_strip_selected
            && !(source.container == ContainerKind::Wave
                && destination == MetadataDestination::Wave);
        Ok(Self {
            policy: policy.clone(),
            evidence,
            source,
            destination,
            wave_chunks,
            wave_facts,
            expected_wave_by_source,
            expected_generated_bwf,
            selected,
            copy_generic,
            plan_bwf,
            clock_changed: source_rate_hz != output_rate_hz || crop_origin_source_frames != 0,
            source_range_complete,
        })
    }

    pub(crate) fn wave_chunks(&self) -> Option<&[WaveChunk]> {
        self.wave_chunks.as_deref()
    }

    pub(crate) fn should_copy_generic(&self) -> bool {
        self.copy_generic
    }

    /// Discover the current destination inventory.  The normalization layer
    /// snapshots this immediately before an authoritative loudness finalizer,
    /// then compares it with the post-finalizer inventory and passes the
    /// resulting exact evidence back into
    /// [`Self::finish_with_bwf_loudness_and_evidence`].
    pub(crate) fn destination_inventory(&self, output: &Path) -> Result<MetadataInventory, String> {
        discover_path(output, DiscoveryLimits::default())
            .map_err(|error| format!("discover output metadata: {error}"))
    }

    /// Return exact destination evidence owned by the output adapter or one
    /// of its explicitly selected loudness writers. The registry's full-region
    /// SHA-256 remains available even when an overlapping container node omits
    /// a second raw allocation.
    pub(crate) fn destination_loudness_evidence(
        &self,
        before: &MetadataInventory,
        after: &MetadataInventory,
        writers: &[DestinationLoudnessWriter],
    ) -> Vec<GeneratedDestinationRegion> {
        if self.destination == MetadataDestination::Wave
            || before.container != after.container
            || !destination_matches(self.destination, after.container)
        {
            return Vec::new();
        }
        let mut exact_after =
            HashMap::<(MetadataKind, Vec<String>, String), VecDeque<usize>>::new();
        for (index, region) in after.regions.iter().enumerate() {
            exact_after
                .entry((
                    region.kind.clone(),
                    region.path.clone(),
                    region.raw_sha256.clone(),
                ))
                .or_default()
                .push_back(index);
        }
        let mut origins = vec![None; after.regions.len()];
        // Metadata already present in the writer preimage was emitted by the
        // selected output adapter. Trust it only when the complete discovered
        // representation survives byte-for-byte; changed regions still need
        // a writer-specific allow-list below.
        for region in &before.regions {
            if let Some(index) = exact_after
                .get_mut(&(
                    region.kind.clone(),
                    region.path.clone(),
                    region.raw_sha256.clone(),
                ))
                .and_then(VecDeque::pop_front)
            {
                origins[index] = Some(GeneratedDestinationOrigin::OutputAdapter);
            }
        }

        let mut previous = HashMap::<(MetadataKind, Vec<String>), VecDeque<String>>::new();
        for region in &before.regions {
            previous
                .entry((region.kind.clone(), region.path.clone()))
                .or_default()
                .push_back(region.raw_sha256.clone());
        }
        for (index, region) in after.regions.iter().enumerate() {
            if !writers
                .iter()
                .any(|writer| destination_writer_owns_region(*writer, after, index))
            {
                continue;
            }
            let prior = previous
                .get_mut(&(region.kind.clone(), region.path.clone()))
                .and_then(VecDeque::pop_front);
            if prior.is_some_and(|digest| digest == region.raw_sha256) {
                continue;
            }
            // `raw_sha256` is computed over the complete region by the
            // registry, including regions whose raw copy is omitted because
            // it overlaps a retained child.  The evidence therefore binds
            // the exact output bytes as well as the writer-owned kind/path.
            origins[index] = Some(GeneratedDestinationOrigin::LoudnessWriter);
        }

        if after.container == ContainerKind::IsoBmff {
            promote_iso_writer_ancestors(after, &mut origins);
        }

        after
            .regions
            .iter()
            .enumerate()
            .filter_map(|(index, region)| {
                origins[index].map(|origin| GeneratedDestinationRegion {
                    kind: region.kind.clone(),
                    locator: region_locator(after.container, region),
                    raw_sha256: region.raw_sha256.clone(),
                    origin,
                })
            })
            .collect()
    }

    /// Add exact evidence for an OpusTags packet that the Opus stream writer
    /// emitted before the loudness finalizer's preimage was captured.
    ///
    /// Opus rendering necessarily writes its mandatory OpusTags packet while
    /// opening the stream, so an unchanged packet is present in both the
    /// preimage and the post-finalizer inventory.  The writer's complete
    /// packet bytes are still available to the caller; bind those bytes to the
    /// discovered OpusTags kind and locator so this narrow case receives the
    /// same exact evidence as a packet changed by the finalizer.  No generic
    /// kind-only promotion is performed here.
    #[cfg(feature = "opus-encoding")]
    pub(crate) fn destination_loudness_evidence_with_opus_tags(
        &self,
        before: &MetadataInventory,
        after: &MetadataInventory,
        expected_tags: &[u8],
    ) -> Vec<GeneratedDestinationRegion> {
        let mut evidence = self.destination_loudness_evidence(
            before,
            after,
            &[DestinationLoudnessWriter::OpusR128],
        );
        if self.destination != MetadataDestination::OggOpus
            || before.container != after.container
            || !destination_matches(self.destination, after.container)
        {
            return evidence;
        }
        let expected_digest = sha256_hex(expected_tags);
        for region in &after.regions {
            if !matches!(region.kind, MetadataKind::OpusTags { .. })
                || region.raw.as_deref() != Some(expected_tags)
                || region.raw_sha256 != expected_digest
            {
                continue;
            }
            let candidate = GeneratedDestinationRegion {
                kind: region.kind.clone(),
                locator: region_locator(after.container, region),
                raw_sha256: region.raw_sha256.clone(),
                origin: GeneratedDestinationOrigin::LoudnessWriter,
            };
            if let Some(existing) = evidence.iter_mut().find(|existing| {
                existing.kind == candidate.kind
                    && existing.locator == candidate.locator
                    && existing.raw_sha256 == candidate.raw_sha256
            }) {
                existing.origin = GeneratedDestinationOrigin::LoudnessWriter;
            } else {
                evidence.push(candidate);
            }
        }
        evidence
    }

    #[cfg(test)]
    pub(crate) fn finish(self, output: &Path) -> Result<MetadataFidelityReport, String> {
        self.finish_with_bwf_loudness(output, None)
    }

    #[cfg(test)]
    pub(crate) fn finish_with_bwf_loudness(
        self,
        output: &Path,
        expected_bwf_fields: Option<[u8; 12]>,
    ) -> Result<MetadataFidelityReport, String> {
        self.finish_with_bwf_loudness_and_evidence(output, expected_bwf_fields, &[])
    }

    pub(crate) fn finish_with_bwf_loudness_and_evidence(
        self,
        output: &Path,
        expected_bwf_fields: Option<[u8; 12]>,
        generated_destination_regions: &[GeneratedDestinationRegion],
    ) -> Result<MetadataFidelityReport, String> {
        let destination = discover_path(output, DiscoveryLimits::default())
            .map_err(|error| format!("discover output metadata: {error}"))?;
        if !destination_matches(self.destination, destination.container) {
            return Err(format!(
                "metadata output container mismatch: requested {:?}, discovered {:?}",
                self.destination, destination.container
            ));
        }
        let entries = self.ledger(
            &destination,
            expected_bwf_fields.as_ref(),
            generated_destination_regions,
        )?;
        MetadataFidelityReport::new(self.policy, self.evidence, entries)
            .map_err(|error| error.to_string())
    }

    fn ledger(
        &self,
        destination: &MetadataInventory,
        expected_bwf_fields: Option<&[u8; 12]>,
        generated_destination_regions: &[GeneratedDestinationRegion],
    ) -> Result<Vec<MetadataLedgerEntry>, String> {
        let mut entries = Vec::new();
        let strip_all = self.policy.policy() == MetadataPolicy::Strip
            && self.policy.strip_scope() == StripScope::All;
        let source_locators = self
            .source
            .regions
            .iter()
            .map(|region| region_locator(self.source.container, region))
            .collect::<Vec<_>>();
        let destination_locators = destination
            .regions
            .iter()
            .map(|region| region_locator(destination.container, region))
            .collect::<Vec<_>>();
        let source_digests = self
            .source
            .regions
            .iter()
            .map(region_digest)
            .collect::<Vec<_>>();
        let destination_digests = destination
            .regions
            .iter()
            .map(region_digest)
            .collect::<Vec<_>>();
        let source_wave_audio_offsets = wave_audio_offsets(&self.source);
        let destination_wave_audio_offsets = wave_audio_offsets(destination);
        let mut destination_matcher =
            DestinationMatcher::new(destination, &destination_locators, &destination_digests);
        let mut wave_facts_by_source = HashMap::<&str, Vec<&WaveFact>>::new();
        for fact in &self.wave_facts {
            wave_facts_by_source
                .entry(fact.source_locator.as_str())
                .or_default()
                .push(fact);
        }
        let updated_bwf_destination = self.plan_bwf.then(|| {
            destination
                .regions
                .iter()
                .position(|region| wave_region_id(region) == Some("bext"))
        });
        let updated_bwf_destination = updated_bwf_destination.flatten();
        // A generated BWF has no source occurrence by construction.  Reserve
        // its exact destination occurrence before any source-backed matching
        // (including requested drops), otherwise a strip request can consume
        // the generated block and misreport it as a surviving source field.
        let generated_bwf_destination =
            self.expected_generated_bwf.as_deref().and_then(|expected| {
                destination
                    .regions
                    .iter()
                    .enumerate()
                    .find_map(|(index, region)| {
                        (wave_region_id(region) == Some("bext")
                            && wave_region_matches_expected(expected, region, expected_bwf_fields))
                        .then_some(index)
                    })
            });
        if let Some(index) = generated_bwf_destination {
            destination_matcher.reserve(index);
        }
        let requested_drop = source_locators
            .iter()
            .map(|locator| strip_all || self.selected.contains(locator))
            .collect::<Vec<_>>();

        // Reserve destination occurrences for every source occurrence that
        // was meant to survive before proving selected/all-scope removals.
        // This makes duplicate accounting a multiset operation: removing the
        // first of two byte-identical chunks does not accidentally consume the
        // one occurrence needed to prove that the second was retained.
        let mut matched_destinations = vec![None; self.source.regions.len()];
        // Exact byte matches must be reserved for every surviving source
        // occurrence before a weaker kind-only mapping is allowed to consume
        // one. Otherwise an earlier changed occurrence of a repeated kind can
        // steal the only exact destination match from a later occurrence.
        for (source_index, source_region) in self.source.regions.iter().enumerate() {
            if requested_drop[source_index] {
                continue;
            }
            if let Some(destination_index) = destination_matcher.take(
                &source_region.kind,
                &source_locators[source_index],
                &source_digests[source_index],
                true,
            ) {
                matched_destinations[source_index] = Some(destination_index);
            }
        }
        for (source_index, source_region) in self.source.regions.iter().enumerate() {
            if requested_drop[source_index] || matched_destinations[source_index].is_some() {
                continue;
            }
            if let Some(destination_index) = destination_matcher.take(
                &source_region.kind,
                &source_locators[source_index],
                &source_digests[source_index],
                false,
            ) {
                matched_destinations[source_index] = Some(destination_index);
            }
        }

        for (source_index, source_region) in self.source.regions.iter().enumerate() {
            let source_locator = source_locators[source_index].clone();
            let before = source_digests[source_index].clone();
            if requested_drop[source_index] {
                // A strip decision is only true when the selected source bytes
                // are absent after all unselected duplicates have been paired.
                // If either the exact representation or a plausible changed
                // representation remains, do not manufacture a Dropped entry.
                // A changed chunk with the same kind/locator is not proof of
                // removal: pairing it conservatively also keeps it from being
                // relabelled below as trusted writer-generated metadata.
                if let Some(destination_index) = destination_matcher
                    .take(&source_region.kind, &source_locator, &before, true)
                    .or_else(|| {
                        destination_matcher.take(
                            &source_region.kind,
                            &source_locator,
                            &before,
                            false,
                        )
                    })
                {
                    let destination_locator = destination_locators[destination_index].clone();
                    let after = destination_digests[destination_index].clone();
                    let same_locator = source_locator == destination_locator;
                    let exact_bytes = before == after;
                    entries.push(MetadataLedgerEntry::new(
                        region_field(source_region),
                        if same_locator && exact_bytes {
                            MetadataOutcome::Preserved
                        } else {
                            MetadataOutcome::Mapped
                        },
                        if same_locator && exact_bytes {
                            MetadataLossClass::None
                        } else if exact_bytes {
                            MetadataLossClass::Representation
                        } else {
                            MetadataLossClass::Semantic
                        },
                        if exact_bytes {
                            "the exact source bytes remain despite the requested metadata strip scope"
                        } else {
                            "a changed destination representation of the selected source field remains, so removal cannot be proven"
                        },
                        Some(source_locator),
                        Some(destination_locator),
                        Some(before.clone()),
                        Some(after),
                    ));
                    continue;
                }
                entries.push(MetadataLedgerEntry::new(
                    region_field(source_region),
                    MetadataOutcome::Dropped,
                    MetadataLossClass::Policy,
                    "removed by the explicit metadata strip scope",
                    Some(source_locator),
                    None,
                    Some(before),
                    None,
                ));
                continue;
            }
            let Some(destination_index) = matched_destinations[source_index] else {
                entries.push(MetadataLedgerEntry::new(
                    region_field(source_region),
                    MetadataOutcome::Dropped,
                    MetadataLossClass::Unsupported,
                    "no verified destination representation was discovered",
                    Some(source_locator),
                    None,
                    Some(before),
                    None,
                ));
                continue;
            };
            let destination_region = &destination.regions[destination_index];
            let destination_locator = destination_locators[destination_index].clone();
            let after = destination_digests[destination_index].clone();
            let placement_changed =
                wave_region_placement(&source_wave_audio_offsets, source_region)
                    != wave_region_placement(&destination_wave_audio_offsets, destination_region);
            let facts = wave_facts_by_source
                .get(source_locator.as_str())
                .map_or(&[][..], Vec::as_slice);
            let candidate = facts.iter().any(|fact| fact.entry.candidate);
            let opaque_timing = self.source.container == ContainerKind::Wave
                && (self.clock_changed || !self.source_range_complete)
                && (!is_supported_wave_region(source_region)
                    || is_opaque_wave_timing_region(source_region));
            let expected_wave = self.expected_wave_by_source.get(&source_locator);
            let first_bwf_destination = self.plan_bwf
                && updated_bwf_destination == Some(destination_index)
                && matches!(source_region.kind, MetadataKind::WaveChunk { ref id, .. } if id == "bext");
            let writer_output_verified = expected_wave.is_some_and(|expected| {
                wave_region_matches_expected(
                    expected,
                    destination_region,
                    if first_bwf_destination {
                        expected_bwf_fields
                    } else {
                        None
                    },
                )
            });
            let strip_ordinal_shift_is_expected = self.policy.policy() == MetadataPolicy::Strip
                && self.policy.strip_scope() == StripScope::Selected
                && source_region.path == destination_region.path
                && destination_index
                    == requested_drop[..source_index]
                        .iter()
                        .filter(|requested| !**requested)
                        .count();
            let known_mapping = !facts.is_empty()
                && facts.iter().all(|fact| {
                    matches!(
                        fact.entry.action,
                        TimingLedgerAction::Mapped | TimingLedgerAction::Preserved
                    )
                })
                && !candidate
                && writer_output_verified;
            let known_bwf_update = first_bwf_destination && writer_output_verified;
            let (outcome, loss_class, reason) = if opaque_timing {
                (
                    MetadataOutcome::Mapped,
                    MetadataLossClass::Semantic,
                    "field bytes were retained, but their placement or sample-domain meaning is not registered",
                )
            } else if before == after
                && !candidate
                && !placement_changed
                && (source_locator == destination_locator || strip_ordinal_shift_is_expected)
            {
                (
                    MetadataOutcome::Preserved,
                    MetadataLossClass::None,
                    if source_locator == destination_locator {
                        "the discovered field bytes were retained"
                    } else {
                        "the exact field bytes were retained at the ordinal implied by the declared earlier removals"
                    },
                )
            } else if before == after && !candidate {
                (
                    MetadataOutcome::Mapped,
                    MetadataLossClass::Representation,
                    if placement_changed {
                        "the field bytes were retained, but their physical placement relative to audio data changed"
                    } else {
                        "the field bytes were retained at a different container locator"
                    },
                )
            } else if candidate {
                (
                    MetadataOutcome::Mapped,
                    MetadataLossClass::Semantic,
                    "bytes were retained, but sample-domain meaning could not be proven",
                )
            } else if known_bwf_update {
                (
                    MetadataOutcome::Mapped,
                    if placement_changed {
                        MetadataLossClass::Representation
                    } else {
                        MetadataLossClass::None
                    },
                    if placement_changed {
                        "BWF production fields were retained and loudness fields were recomputed, but chunk placement relative to audio data changed"
                    } else {
                        "BWF production fields were retained and output loudness fields were recomputed"
                    },
                )
            } else if known_mapping {
                (
                    MetadataOutcome::Mapped,
                    if placement_changed {
                        MetadataLossClass::Representation
                    } else {
                        MetadataLossClass::None
                    },
                    if placement_changed {
                        "sample-indexed values were mapped exactly, but chunk placement relative to audio data changed"
                    } else {
                        "sample-indexed values were mapped with checked rational arithmetic"
                    },
                )
            } else {
                (
                    MetadataOutcome::Mapped,
                    MetadataLossClass::Representation,
                    "the destination representation changed and only round-trip presence was verified",
                )
            };
            for fact in facts {
                entries.push(timing_entry(
                    fact,
                    &source_locator,
                    &destination_locator,
                    &before,
                    &after,
                    writer_output_verified,
                ));
            }
            if known_bwf_update {
                append_bwf_recomputed_entries(
                    &mut entries,
                    Some(source_region),
                    destination_region,
                    &source_locator,
                    &destination_locator,
                )?;
            }
            entries.push(MetadataLedgerEntry::new(
                region_field(source_region),
                outcome,
                loss_class,
                reason,
                Some(source_locator.clone()),
                Some(destination_locator.clone()),
                Some(before),
                Some(after),
            ));
        }

        for (index, region) in destination.regions.iter().enumerate() {
            if destination_matcher.is_used(index) && Some(index) != generated_bwf_destination {
                continue;
            }
            let destination_locator = region_locator(destination.container, region);
            let trusted_bwf_generation = generated_bwf_destination == Some(index)
                && self
                    .expected_generated_bwf
                    .as_deref()
                    .is_some_and(|expected| {
                        wave_region_matches_expected(expected, region, expected_bwf_fields)
                    });
            let trusted_destination_generation =
                generated_destination_regions.iter().find(|evidence| {
                    evidence.kind == region.kind
                        && evidence.locator == destination_locator
                        && evidence.raw_sha256 == region.raw_sha256
                });
            if trusted_bwf_generation {
                append_bwf_recomputed_entries(
                    &mut entries,
                    None,
                    region,
                    "",
                    &destination_locator,
                )?;
            }
            entries.push(MetadataLedgerEntry::new(
                region_field(region),
                MetadataOutcome::Recomputed,
                if trusted_bwf_generation || trusted_destination_generation.is_some() {
                    MetadataLossClass::None
                } else {
                    MetadataLossClass::Unsupported
                },
                if trusted_bwf_generation {
                    "generated by the explicitly selected BWF loudness metadata finalizer"
                } else if trusted_destination_generation.is_some_and(|evidence| {
                    evidence.origin == GeneratedDestinationOrigin::OutputAdapter
                }) {
                    "generated by the authoritative output adapter and retained byte-for-byte through metadata finalization"
                } else if trusted_destination_generation.is_some_and(|evidence| {
                    evidence.origin == GeneratedDestinationOrigin::ContainerAncestor
                }) {
                    "rewritten by the authoritative output metadata writer; every direct child is bound to exact adapter or writer evidence"
                } else if trusted_destination_generation.is_some() {
                    "generated by the authoritative output loudness metadata writer; exact kind, locator, and bytes were verified"
                } else {
                    "destination-only metadata was discovered without writer-specific generation evidence"
                },
                None,
                Some(destination_locator),
                None,
                Some(region_digest(region)),
            ));
        }
        for (index, issue) in self.source.issues.iter().enumerate() {
            let serialized = serde_json::to_vec(issue)
                .map_err(|error| format!("encode source metadata issue: {error}"))?;
            entries.push(MetadataLedgerEntry::new(
                format!("registry_issue.source.{}", issue.code),
                MetadataOutcome::Dropped,
                if strip_all {
                    MetadataLossClass::Policy
                } else {
                    MetadataLossClass::Unsupported
                },
                issue.message.clone(),
                Some(format!(
                    "{}:issue:{}@{}/#{}",
                    container_name(self.source.container),
                    issue.code,
                    issue
                        .offset
                        .map_or_else(|| "unknown".into(), |value| value.to_string()),
                    index,
                )),
                None,
                Some(sha256_hex(&serialized)),
                None,
            ));
        }
        for (index, issue) in destination.issues.iter().enumerate() {
            let serialized = serde_json::to_vec(issue)
                .map_err(|error| format!("encode output metadata issue: {error}"))?;
            entries.push(MetadataLedgerEntry::new(
                format!("registry_issue.destination.{}", issue.code),
                MetadataOutcome::Recomputed,
                MetadataLossClass::Unsupported,
                issue.message.clone(),
                None,
                Some(format!(
                    "{}:issue:{}@{}/#{}",
                    container_name(destination.container),
                    issue.code,
                    issue
                        .offset
                        .map_or_else(|| "unknown".into(), |value| value.to_string()),
                    index,
                )),
                None,
                Some(sha256_hex(&serialized)),
            ));
        }
        Ok(entries)
    }
}

fn append_bwf_recomputed_entries(
    entries: &mut Vec<MetadataLedgerEntry>,
    source: Option<&MetadataRegion>,
    destination: &MetadataRegion,
    source_chunk_locator: &str,
    destination_chunk_locator: &str,
) -> Result<(), String> {
    const FIELDS: [(&str, usize); 6] = [
        ("Version", 346),
        ("LoudnessValue", 412),
        ("LoudnessRange", 414),
        ("MaxTruePeakLevel", 416),
        ("MaxMomentaryLoudness", 418),
        ("MaxShortTermLoudness", 420),
    ];
    let source_body = source
        .and_then(|source| source.raw.as_deref())
        .and_then(|raw| encoded_wave_chunk_body(raw).ok());
    let destination_raw = destination
        .raw
        .as_deref()
        .ok_or("BWF output bytes were not retained by the bounded metadata registry")?;
    let destination_body = encoded_wave_chunk_body(destination_raw)?;
    for (field, offset) in FIELDS {
        let end = offset + 2;
        let after = destination_body
            .get(offset..end)
            .ok_or_else(|| format!("BWF output bext is too short for recomputed field {field}"))?;
        let before = source_body.and_then(|body| body.get(offset..end));
        entries.push(MetadataLedgerEntry::new(
            format!("wave_bwf.{field}"),
            MetadataOutcome::Recomputed,
            MetadataLossClass::None,
            "recomputed from the authoritative measured output for BWF v2 publication",
            before.map(|_| format!("{source_chunk_locator}/field/{field}")),
            Some(format!("{destination_chunk_locator}/field/{field}")),
            before.map(sha256_hex),
            Some(sha256_hex(after)),
        ));
    }
    Ok(())
}

fn destination_writer_owns_region(
    writer: DestinationLoudnessWriter,
    destination: &MetadataInventory,
    index: usize,
) -> bool {
    let Some(region) = destination.regions.get(index) else {
        return false;
    };
    match writer {
        DestinationLoudnessWriter::ReplayGain => replaygain_region(destination, index, region),
        DestinationLoudnessWriter::OpusR128 => opus_tags_region(region),
        DestinationLoudnessWriter::IsoBmffNative => {
            destination.container == ContainerKind::IsoBmff
                && native_iso_loudness_region(destination, index, region)
        }
        DestinationLoudnessWriter::IsoBmffSoundCheck => {
            destination.container == ContainerKind::IsoBmff
                && sound_check_iso_region(destination, index, region)
        }
    }
}

fn loudness_writers_for_destination(
    destination: MetadataDestination,
    sound_check_requested: bool,
) -> Vec<DestinationLoudnessWriter> {
    match destination {
        MetadataDestination::Flac | MetadataDestination::OggVorbis => {
            vec![DestinationLoudnessWriter::ReplayGain]
        }
        MetadataDestination::OggOpus => vec![DestinationLoudnessWriter::OpusR128],
        MetadataDestination::IsoBmff => {
            let mut writers = vec![
                DestinationLoudnessWriter::ReplayGain,
                DestinationLoudnessWriter::IsoBmffNative,
            ];
            if sound_check_requested {
                writers.push(DestinationLoudnessWriter::IsoBmffSoundCheck);
            }
            writers
        }
        // The current MP3 and WAVE metadata-only writers do not expose a
        // registry-native loudness region whose semantics can be bound here.
        MetadataDestination::Mp3 | MetadataDestination::Wave => Vec::new(),
    }
}

/// Promote only ISO metadata container ancestors whose complete set of direct
/// children is already bound either to the exact output-adapter preimage or to
/// an explicitly owned writer result. This lets strict publication account for
/// size/header changes in `udta/meta/ilst` without turning those broad parent
/// boxes into a kind-only allow-list.
fn promote_iso_writer_ancestors(
    destination: &MetadataInventory,
    origins: &mut [Option<GeneratedDestinationOrigin>],
) {
    let mut regions_by_path = HashMap::<Vec<String>, Vec<(u64, u64, usize)>>::new();
    for (index, region) in destination.regions.iter().enumerate() {
        let Some((start, end)) = region_interval(region) else {
            continue;
        };
        regions_by_path
            .entry(region.path.clone())
            .or_default()
            .push((start, end, index));
    }
    for regions in regions_by_path.values_mut() {
        regions.sort_unstable_by_key(|(start, end, index)| (*start, *end, *index));
    }

    let mut direct_children = vec![Vec::<usize>::new(); destination.regions.len()];
    for (child_index, child) in destination.regions.iter().enumerate() {
        if child.path.len() < 2 {
            continue;
        }
        let Some((child_start, child_end)) = region_interval(child) else {
            continue;
        };
        let Some(parents) = regions_by_path.get(&child.path[..child.path.len() - 1]) else {
            continue;
        };
        let position = parents.partition_point(|(start, _, _)| *start <= child_start);
        let Some((parent_start, parent_end, parent_index)) =
            position.checked_sub(1).and_then(|index| parents.get(index))
        else {
            continue;
        };
        if *parent_start <= child_start && child_end <= *parent_end {
            direct_children[*parent_index].push(child_index);
        }
    }

    let mut candidates = destination
        .regions
        .iter()
        .enumerate()
        .filter_map(|(index, region)| match &region.kind {
            MetadataKind::IsoBmffBox { id, .. }
                if matches!(id.as_str(), "udta" | "meta" | "ilst") =>
            {
                Some(index)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    candidates
        .sort_unstable_by_key(|index| std::cmp::Reverse(destination.regions[*index].path.len()));
    for index in candidates {
        if origins[index].is_some() || direct_children[index].is_empty() {
            continue;
        }
        let children = &direct_children[index];
        if children.iter().all(|child| origins[*child].is_some())
            && children.iter().any(|child| {
                matches!(
                    origins[*child],
                    Some(
                        GeneratedDestinationOrigin::LoudnessWriter
                            | GeneratedDestinationOrigin::ContainerAncestor
                    )
                )
            })
        {
            origins[index] = Some(GeneratedDestinationOrigin::ContainerAncestor);
        }
    }
}

fn region_interval(region: &MetadataRegion) -> Option<(u64, u64)> {
    let first = region.extents.first()?;
    if region.extents.len() != 1 {
        return None;
    }
    first
        .offset
        .checked_add(first.length)
        .map(|end| (first.offset, end))
}

fn replaygain_region(
    destination: &MetadataInventory,
    index: usize,
    region: &MetadataRegion,
) -> bool {
    match (&destination.container, &region.kind) {
        (ContainerKind::Flac, MetadataKind::FlacBlock { block_type }) => {
            (*block_type == 4 && region.path == ["fLaC".to_string(), "block:4".to_string()])
                || (*block_type == 1 && flac_zero_padding_region(region))
        }
        (
            ContainerKind::OggVorbis,
            MetadataKind::VorbisComment {
                serial,
                packet_index,
            },
        ) => ogg_comment_region(&region.path, *serial, *packet_index),
        (ContainerKind::IsoBmff, MetadataKind::IsoBmffBox { id, .. }) => {
            replaygain_iso_region(destination, index, region, id)
        }
        _ => false,
    }
}

fn flac_zero_padding_region(region: &MetadataRegion) -> bool {
    if region.path != ["fLaC".to_string(), "block:1".to_string()] {
        return false;
    }
    let Some(raw) = region.raw.as_deref() else {
        return false;
    };
    if raw.len() < 4 || raw[0] & 0x7f != 1 {
        return false;
    }
    let declared = (usize::from(raw[1]) << 16) | (usize::from(raw[2]) << 8) | usize::from(raw[3]);
    declared == raw.len() - 4 && raw[4..].iter().all(|byte| *byte == 0)
}

fn opus_tags_region(region: &MetadataRegion) -> bool {
    match &region.kind {
        MetadataKind::OpusTags {
            serial,
            packet_index,
        } => ogg_comment_region(&region.path, *serial, *packet_index),
        _ => false,
    }
}

fn ogg_comment_region(path: &[String], serial: u32, packet_index: u32) -> bool {
    path == [
        "Ogg".to_string(),
        format!("serial:{serial}"),
        format!("packet:{packet_index}"),
    ] && packet_index == 1
}

fn replaygain_iso_region(
    destination: &MetadataInventory,
    index: usize,
    region: &MetadataRegion,
    id: &str,
) -> bool {
    let Some(item_path) = freeform_item_path(&region.path, id) else {
        return false;
    };
    if !item_path
        .windows(2)
        .any(|window| window == ["meta", "ilst"])
    {
        return false;
    }
    let Some(item_index) = freeform_item_index(destination, index, item_path, id) else {
        return false;
    };
    let Some(children) = replaygain_freeform_children(destination, item_index) else {
        return false;
    };
    index == item_index || children.contains(&index)
}

fn sound_check_iso_region(
    destination: &MetadataInventory,
    index: usize,
    region: &MetadataRegion,
) -> bool {
    let MetadataKind::IsoBmffBox { id, .. } = &region.kind else {
        return false;
    };
    let Some(item_path) = freeform_item_path(&region.path, id) else {
        return false;
    };
    if !item_path
        .windows(2)
        .any(|window| window == ["meta", "ilst"])
    {
        return false;
    }
    let Some(item_index) = freeform_item_index(destination, index, item_path, id) else {
        return false;
    };
    let Some(children) = sound_check_freeform_children(destination, item_index) else {
        return false;
    };
    index == item_index || children.contains(&index)
}

fn freeform_item_path<'a>(path: &'a [String], id: &str) -> Option<&'a [String]> {
    match id {
        "----" if path.last().is_some_and(|component| component == "----") => Some(path),
        "mean" | "name" | "data"
            if path.len() >= 2
                && path.last().is_some_and(|component| component == id)
                && path[path.len() - 2] == "----" =>
        {
            Some(&path[..path.len() - 1])
        }
        _ => None,
    }
}

fn freeform_item_index(
    destination: &MetadataInventory,
    index: usize,
    item_path: &[String],
    id: &str,
) -> Option<usize> {
    if id == "----" {
        return Some(index);
    }
    let child = destination.regions.get(index)?;
    let mut parents = destination
        .regions
        .iter()
        .enumerate()
        .filter(|(_, candidate)| {
            candidate.path == item_path
                && matches!(
                    &candidate.kind,
                    MetadataKind::IsoBmffBox { id, .. } if id == "----"
                )
                && iso_region_contains(candidate, child)
        });
    let (parent_index, _) = parents.next()?;
    parents.next().is_none().then_some(parent_index)
}

fn replaygain_freeform_children(
    destination: &MetadataInventory,
    item_index: usize,
) -> Option<[usize; 3]> {
    let item = destination.regions.get(item_index)?;
    let mut mean = None;
    let mut name = None;
    let mut data = None;
    let mut child_count = 0_usize;
    for (index, child) in destination.regions.iter().enumerate() {
        if !is_direct_iso_child(item, child) {
            continue;
        }
        child_count += 1;
        let MetadataKind::IsoBmffBox { id, .. } = &child.kind else {
            return None;
        };
        match id.as_str() {
            "mean"
                if mean.is_none()
                    && bmff_text_value(child.raw.as_deref()) == Some(b"com.apple.iTunes") =>
            {
                mean = Some(index);
            }
            "name"
                if name.is_none()
                    && bmff_text_value(child.raw.as_deref())
                        .is_some_and(is_replaygain_freeform_name) =>
            {
                name = Some(index);
            }
            "data" if data.is_none() && valid_bmff_leaf(child.raw.as_deref(), b"data") => {
                data = Some(index);
            }
            _ => return None,
        }
    }
    (child_count == 3).then_some([mean?, name?, data?])
}

fn sound_check_freeform_children(
    destination: &MetadataInventory,
    item_index: usize,
) -> Option<[usize; 3]> {
    let item = destination.regions.get(item_index)?;
    let mut mean = None;
    let mut name = None;
    let mut data = None;
    let mut child_count = 0_usize;
    for (index, child) in destination.regions.iter().enumerate() {
        if !is_direct_iso_child(item, child) {
            continue;
        }
        child_count += 1;
        let MetadataKind::IsoBmffBox { id, .. } = &child.kind else {
            return None;
        };
        match id.as_str() {
            "mean"
                if mean.is_none()
                    && bmff_text_value(child.raw.as_deref()) == Some(b"com.apple.iTunes") =>
            {
                mean = Some(index);
            }
            "name"
                if name.is_none() && bmff_text_value(child.raw.as_deref()) == Some(b"iTunNORM") =>
            {
                name = Some(index);
            }
            "data" if data.is_none() && sound_check_data_leaf(child.raw.as_deref()) => {
                data = Some(index);
            }
            _ => return None,
        }
    }
    (child_count == 3).then_some([mean?, name?, data?])
}

fn valid_bmff_leaf(raw: Option<&[u8]>, id: &[u8; 4]) -> bool {
    let Some(raw) = raw else {
        return false;
    };
    raw.len() >= 8
        && &raw[4..8] == id
        && usize::try_from(u32::from_be_bytes(raw[..4].try_into().unwrap())) == Ok(raw.len())
}

fn sound_check_data_leaf(raw: Option<&[u8]>) -> bool {
    let Some(payload) = bmff_data_payload(raw) else {
        return false;
    };
    std::str::from_utf8(payload)
        .ok()
        .and_then(|value| crate::metadata::SoundCheck::parse(value).ok())
        .is_some()
}

fn bmff_data_payload(raw: Option<&[u8]>) -> Option<&[u8]> {
    let raw = raw?;
    if raw.len() < 16
        || &raw[4..8] != b"data"
        || usize::try_from(u32::from_be_bytes(raw[..4].try_into().unwrap())) != Ok(raw.len())
    {
        return None;
    }
    Some(&raw[16..])
}

fn iso_region_contains(parent: &MetadataRegion, child: &MetadataRegion) -> bool {
    match (region_interval(parent), region_interval(child)) {
        (Some((parent_start, parent_end)), Some((child_start, child_end))) => {
            parent_start <= child_start && child_end <= parent_end
        }
        _ => false,
    }
}

fn is_direct_iso_child(parent: &MetadataRegion, child: &MetadataRegion) -> bool {
    child.path.len() == parent.path.len() + 1
        && child.path.starts_with(&parent.path)
        && iso_region_contains(parent, child)
}

fn bmff_text_value(raw: Option<&[u8]>) -> Option<&[u8]> {
    let raw = raw?;
    if raw.len() < 12 {
        return None;
    }
    let size = u32::from_be_bytes(raw[..4].try_into().ok()?) as usize;
    if size != raw.len() || &raw[4..8] != b"mean" && &raw[4..8] != b"name" {
        return None;
    }
    Some(&raw[12..])
}

fn is_replaygain_freeform_name(name: &[u8]) -> bool {
    matches!(
        name,
        b"replaygain_track_gain"
            | b"replaygain_track_peak"
            | b"replaygain_album_gain"
            | b"replaygain_album_peak"
    )
}

fn native_iso_loudness_region(
    destination: &MetadataInventory,
    index: usize,
    region: &MetadataRegion,
) -> bool {
    let MetadataKind::IsoBmffBox { id, .. } = &region.kind else {
        return false;
    };
    let track_udta = region
        .path
        .windows(2)
        .any(|window| window == ["trak", "udta"]);
    if !track_udta {
        return false;
    }
    match id.as_str() {
        "udta" => has_direct_iso_child(destination, index, "ludt"),
        "ludt" => has_direct_iso_child(destination, index, "tlou"),
        "tlou" | "alou" => path_ends_with(&region.path, &["udta", "ludt", id.as_str()]),
        _ => false,
    }
}

fn has_direct_iso_child(destination: &MetadataInventory, index: usize, child: &str) -> bool {
    let Some(parent) = destination.regions.get(index) else {
        return false;
    };
    destination.regions.iter().any(|candidate| {
        is_direct_iso_child(parent, candidate)
            && candidate.path.last().is_some_and(|id| id == child)
    })
}

fn path_ends_with(path: &[String], suffix: &[&str]) -> bool {
    path.len() >= suffix.len()
        && path[path.len() - suffix.len()..]
            .iter()
            .zip(suffix)
            .all(|(actual, expected)| actual == expected)
}

#[derive(Default)]
struct DestinationMatcher {
    used: Vec<bool>,
    kind_ids: HashMap<MetadataKind, usize>,
    digest_ids: HashMap<String, usize>,
    locator_ids: HashMap<String, usize>,
    exact: HashMap<(usize, usize), VecDeque<usize>>,
    exact_locator: HashMap<(usize, usize, usize), VecDeque<usize>>,
    by_kind: HashMap<usize, VecDeque<usize>>,
    by_kind_locator: HashMap<(usize, usize), VecDeque<usize>>,
}

impl DestinationMatcher {
    fn new(destination: &MetadataInventory, locators: &[String], digests: &[String]) -> Self {
        let mut matcher = Self {
            used: vec![false; destination.regions.len()],
            ..Self::default()
        };
        for (index, region) in destination.regions.iter().enumerate() {
            let kind_id = intern(&mut matcher.kind_ids, region.kind.clone());
            let digest_id = intern(&mut matcher.digest_ids, digests[index].clone());
            let locator_id = intern(&mut matcher.locator_ids, locators[index].clone());
            matcher
                .exact
                .entry((kind_id, digest_id))
                .or_default()
                .push_back(index);
            matcher
                .exact_locator
                .entry((kind_id, digest_id, locator_id))
                .or_default()
                .push_back(index);
            matcher.by_kind.entry(kind_id).or_default().push_back(index);
            matcher
                .by_kind_locator
                .entry((kind_id, locator_id))
                .or_default()
                .push_back(index);
        }
        matcher
    }

    /// Consume the same deterministic candidate selected by the former
    /// whole-inventory scan: exact bytes first, then the same locator, then
    /// physical destination order.
    fn take(
        &mut self,
        kind: &MetadataKind,
        locator: &str,
        digest: &str,
        exact_bytes_only: bool,
    ) -> Option<usize> {
        let kind_id = *self.kind_ids.get(kind)?;
        let digest_id = self.digest_ids.get(digest).copied();
        let locator_id = self.locator_ids.get(locator).copied();
        let mut selected = None;
        if let Some(digest_id) = digest_id {
            if let Some(locator_id) = locator_id {
                selected = take_first_unused(
                    self.exact_locator
                        .get_mut(&(kind_id, digest_id, locator_id)),
                    &self.used,
                );
            }
            if selected.is_none() {
                selected = take_first_unused(self.exact.get_mut(&(kind_id, digest_id)), &self.used);
            }
        }
        if !exact_bytes_only && selected.is_none() {
            if let Some(locator_id) = locator_id {
                selected = take_first_unused(
                    self.by_kind_locator.get_mut(&(kind_id, locator_id)),
                    &self.used,
                );
            }
            if selected.is_none() {
                selected = take_first_unused(self.by_kind.get_mut(&kind_id), &self.used);
            }
        }
        if let Some(index) = selected {
            self.used[index] = true;
        }
        selected
    }

    fn is_used(&self, index: usize) -> bool {
        self.used[index]
    }

    fn reserve(&mut self, index: usize) {
        if let Some(used) = self.used.get_mut(index) {
            *used = true;
        }
    }
}

fn intern<T>(values: &mut HashMap<T, usize>, value: T) -> usize
where
    T: Clone + Eq + std::hash::Hash,
{
    if let Some(id) = values.get(&value) {
        *id
    } else {
        let id = values.len();
        values.insert(value, id);
        id
    }
}

fn take_first_unused(queue: Option<&mut VecDeque<usize>>, used: &[bool]) -> Option<usize> {
    let queue = queue?;
    while queue.front().is_some_and(|index| used[*index]) {
        queue.pop_front();
    }
    queue.pop_front()
}

fn timing_entry(
    fact: &WaveFact,
    source_chunk: &str,
    destination_chunk: &str,
    source_chunk_sha256: &str,
    destination_chunk_sha256: &str,
    writer_output_verified: bool,
) -> MetadataLedgerEntry {
    let field_locator = sanitize_field_locator(&fact.entry.field);
    let source_locator = format!("{source_chunk}/field/{field_locator}");
    let destination_locator = format!("{destination_chunk}/field/{field_locator}");
    let mut reason = match fact.entry.rounding {
        Some(rounding) => format!("{}; rounding={rounding:?}", fact.entry.detail),
        None => fact.entry.detail.clone(),
    };
    let output_mismatch = !writer_output_verified
        && !fact.entry.candidate
        && fact.entry.action != TimingLedgerAction::Dropped;
    if output_mismatch {
        reason.push_str(
            "; completed output bytes do not match the writer-specific timing transformation",
        );
    }
    let (outcome, loss_class) = if fact.entry.candidate || output_mismatch {
        (MetadataOutcome::Mapped, MetadataLossClass::Semantic)
    } else {
        match fact.entry.action {
            TimingLedgerAction::Mapped => (MetadataOutcome::Mapped, MetadataLossClass::None),
            TimingLedgerAction::Preserved => (MetadataOutcome::Preserved, MetadataLossClass::None),
            TimingLedgerAction::Dropped => (MetadataOutcome::Dropped, MetadataLossClass::Policy),
        }
    };
    // Prefer the timing adapter's hashes of the exact fixed-width field
    // bytes.  Opaque/chunk-level decisions deliberately have no proven field
    // boundary, so their truthful byte identity is the containing metadata
    // region.  Preserve candidates retain their identified field bytes
    // unchanged even though the timing adapter cannot prove their semantics.
    let before_sha256 = fact
        .entry
        .source_field_sha256()
        .unwrap_or(source_chunk_sha256)
        .to_owned();
    let after_sha256 = if outcome == MetadataOutcome::Dropped {
        None
    } else if output_mismatch {
        // The expected transformed field hash is not evidence about a
        // different completed output. Bind this failed proof to the actual
        // containing destination region instead.
        Some(destination_chunk_sha256.to_owned())
    } else {
        Some(
            fact.entry
                .destination_field_sha256()
                .or_else(|| {
                    (fact.entry.action == TimingLedgerAction::Preserved)
                        .then(|| fact.entry.source_field_sha256())
                        .flatten()
                })
                .unwrap_or(destination_chunk_sha256)
                .to_owned(),
        )
    };
    MetadataLedgerEntry::new(
        format!(
            "wave_timing.{}.{}",
            String::from_utf8_lossy(&fact.entry.chunk_id),
            fact.entry.field
        ),
        outcome,
        loss_class,
        reason,
        Some(source_locator),
        (outcome != MetadataOutcome::Dropped).then_some(destination_locator),
        Some(before_sha256),
        after_sha256,
    )
}

fn sanitize_field_locator(field: &str) -> String {
    field
        .chars()
        .map(|character| {
            if character.is_control() {
                '_'
            } else {
                character
            }
        })
        .collect()
}

fn destination_matches(expected: MetadataDestination, actual: ContainerKind) -> bool {
    matches!(
        (expected, actual),
        (MetadataDestination::Wave, ContainerKind::Wave)
            | (MetadataDestination::Flac, ContainerKind::Flac)
            | (MetadataDestination::Mp3, ContainerKind::Mp3)
            | (MetadataDestination::OggOpus, ContainerKind::OggOpus)
            | (MetadataDestination::OggVorbis, ContainerKind::OggVorbis)
            | (MetadataDestination::IsoBmff, ContainerKind::IsoBmff)
    )
}

fn destination_for_container(container: ContainerKind) -> Result<MetadataDestination, String> {
    match container {
        ContainerKind::Wave => Ok(MetadataDestination::Wave),
        ContainerKind::Flac => Ok(MetadataDestination::Flac),
        ContainerKind::Mp3 => Ok(MetadataDestination::Mp3),
        ContainerKind::OggVorbis => Ok(MetadataDestination::OggVorbis),
        ContainerKind::OggOpus => Ok(MetadataDestination::OggOpus),
        ContainerKind::IsoBmff => Ok(MetadataDestination::IsoBmff),
        ContainerKind::Ogg => {
            Err("metadata comparison cannot identify the codec carried by the Ogg container".into())
        }
    }
}

fn region_locator(container: ContainerKind, region: &MetadataRegion) -> String {
    const MAX_LOCATOR_BYTES: usize = 1024;

    // JSON array encoding preserves path-component boundaries even when a
    // container-native identifier itself contains '/', quotes, or backslashes.
    // The physical ordinal makes every occurrence unique within an inventory.
    let encoded_path = serde_json::to_string(&region.path)
        .expect("metadata registry paths contain only serializable strings");
    let locator = format!(
        "{}:{encoded_path}/#{}",
        container_name(container),
        region.ordinal
    );
    if locator.len() <= MAX_LOCATOR_BYTES {
        locator
    } else {
        // Registry path components are individually bounded but a deeply
        // nested path can exceed the report locator ceiling. The ordinal still
        // supplies collision-free occurrence identity inside this inventory;
        // the digest retains a stable binding to the complete canonical path.
        format!(
            "{}:path-sha256:{}/#{}",
            container_name(container),
            sha256_hex(encoded_path.as_bytes()),
            region.ordinal
        )
    }
}

fn region_field(region: &MetadataRegion) -> String {
    let encoded = serde_json::to_string(&region.kind).unwrap_or_else(|_| "unknown".into());
    let field = format!("container_region.{encoded}");
    if field.len() <= 256 {
        field
    } else {
        // Container identifiers such as a valid 255-byte APEv2 key can expand
        // beyond the report contract's 256-byte field-name ceiling once JSON
        // escaped. Keep the identity deterministic without truncating UTF-8 or
        // making an otherwise valid source impossible to publish.
        format!("container_region.sha256.{}", sha256_hex(encoded.as_bytes()))
    }
}

fn container_name(container: ContainerKind) -> &'static str {
    match container {
        ContainerKind::Wave => "wave",
        ContainerKind::Flac => "flac",
        ContainerKind::Mp3 => "mp3",
        ContainerKind::OggVorbis => "ogg_vorbis",
        ContainerKind::OggOpus => "ogg_opus",
        ContainerKind::Ogg => "ogg",
        ContainerKind::IsoBmff => "iso_bmff",
    }
}

fn wave_audio_offsets(inventory: &MetadataInventory) -> Vec<u64> {
    if inventory.container != ContainerKind::Wave {
        return Vec::new();
    }
    inventory
        .structural_regions
        .iter()
        .filter(|region| {
            matches!(
                region.kind,
                crate::metadata_registry::StructuralKind::WaveChunk {
                    audio_data: true,
                    ..
                }
            )
        })
        .filter_map(|region| region.extents.iter().map(|extent| extent.offset).min())
        .collect()
}

fn wave_region_placement(audio_offsets: &[u64], region: &MetadataRegion) -> Option<(usize, usize)> {
    if audio_offsets.is_empty() {
        return None;
    }
    let first_offset = region.extents.iter().map(|extent| extent.offset).min()?;
    Some((
        audio_offsets.partition_point(|audio_offset| *audio_offset < first_offset),
        audio_offsets.len(),
    ))
}

fn wave_region_matches_expected(
    expected: &[u8],
    actual: &MetadataRegion,
    expected_bwf_fields: Option<&[u8; 12]>,
) -> bool {
    let Some(actual) = actual.raw.as_deref() else {
        return false;
    };
    if expected.len() != actual.len() {
        return false;
    }
    let Some(fields) = expected_bwf_fields else {
        return expected == actual;
    };
    // Compare the completed BWF bytes with the exact authoritative values
    // derived from the measured output. Every header, production byte,
    // padding byte, and timing field remains fixed too.
    expected
        .iter()
        .zip(actual)
        .enumerate()
        .all(|(index, pair)| {
            let authoritative = match index.checked_sub(8) {
                Some(346..=347) => Some(fields[index - (8 + 346)]),
                Some(412..=421) => Some(fields[2 + index - (8 + 412)]),
                _ => None,
            };
            authoritative.unwrap_or(*pair.0) == *pair.1
        })
}

fn region_digest(region: &MetadataRegion) -> String {
    region.raw_sha256.clone()
}

fn wave_region_id(region: &MetadataRegion) -> Option<&str> {
    match &region.kind {
        MetadataKind::WaveChunk { id, .. } => Some(id),
        _ => None,
    }
}

fn is_supported_wave_region(region: &MetadataRegion) -> bool {
    wave_region_id(region).is_some_and(|id| {
        matches!(
            id,
            "bext" | "axml" | "bxml" | "sxml" | "chna" | "iXML" | "cue " | "LIST" | "smpl"
        )
    })
}

fn is_opaque_wave_timing_region(region: &MetadataRegion) -> bool {
    wave_region_id(region).is_some_and(|id| matches!(id, "axml" | "bxml" | "sxml" | "iXML"))
}

fn validate_encoded_wave_chunk(raw: &[u8], locator: &str) -> Result<(), String> {
    encoded_wave_chunk_body(raw)
        .map(drop)
        .map_err(|error| format!("invalid retained WAVE metadata region {locator}: {error}"))
}

fn encoded_wave_chunk_body(raw: &[u8]) -> Result<&[u8], String> {
    if raw.len() < 8 {
        return Err("chunk header is truncated".into());
    }
    let body_len = u32::from_le_bytes(raw[4..8].try_into().unwrap()) as usize;
    let expected = 8_usize
        .checked_add(body_len)
        .and_then(|value| value.checked_add(body_len & 1))
        .ok_or("chunk size overflow")?;
    if raw.len() != expected {
        return Err(format!(
            "encoded extent has {} bytes, expected {expected}",
            raw.len()
        ));
    }
    Ok(&raw[8..8 + body_len])
}

fn encode_wave_chunk(chunk: &WaveChunk) -> Result<Vec<u8>, String> {
    let body_len = u32::try_from(chunk.body.len())
        .map_err(|_| "WAVE metadata chunk exceeds the u32 chunk-size field".to_string())?;
    let mut encoded = Vec::with_capacity(8 + chunk.body.len() + (chunk.body.len() & 1));
    encoded.extend_from_slice(&chunk.id);
    encoded.extend_from_slice(&body_len.to_le_bytes());
    encoded.extend_from_slice(&chunk.body);
    if chunk.body.len() & 1 != 0 {
        encoded.push(0);
    }
    Ok(encoded)
}

fn synthetic_wave(chunks: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    let encoded_len = chunks.iter().try_fold(20_usize, |total, chunk| {
        total
            .checked_add(chunk.len())
            .ok_or_else(|| "WAVE metadata aggregate size overflow".to_string())
    })?;
    let riff_size = u32::try_from(encoded_len - 8)
        .map_err(|_| "WAVE metadata aggregate exceeds RIFF bounds".to_string())?;
    let mut bytes = Vec::with_capacity(encoded_len);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&riff_size.to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    for chunk in chunks {
        bytes.extend_from_slice(chunk);
    }
    bytes.extend_from_slice(b"data\0\0\0\0");
    Ok(bytes)
}

fn encoded_wave_chunks(bytes: &[u8]) -> Result<Vec<&[u8]>, String> {
    if bytes.len() < 20 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("timing adapter returned an invalid synthetic WAVE stream".into());
    }
    let mut chunks = Vec::new();
    let mut offset = 12_usize;
    while offset < bytes.len() {
        let header_end = offset
            .checked_add(8)
            .ok_or("synthetic WAVE chunk offset overflow")?;
        if header_end > bytes.len() {
            return Err("synthetic WAVE chunk header is truncated".into());
        }
        let body_len =
            u32::from_le_bytes(bytes[offset + 4..header_end].try_into().unwrap()) as usize;
        let end = header_end
            .checked_add(body_len)
            .and_then(|value| value.checked_add(body_len & 1))
            .ok_or("synthetic WAVE chunk size overflow")?;
        if end > bytes.len() {
            return Err("synthetic WAVE chunk exceeds its container".into());
        }
        if &bytes[offset..offset + 4] != b"data" {
            chunks.push(&bytes[offset..end]);
        }
        offset = end;
    }
    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn push_chunk(bytes: &mut Vec<u8>, id: [u8; 4], body: &[u8]) {
        bytes.extend_from_slice(&id);
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(body);
        if body.len() & 1 != 0 {
            bytes.push(0);
        }
    }

    fn wave(rate: u32, chunks: &[WaveChunk]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF\0\0\0\0WAVE");
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&1_u16.to_le_bytes());
        fmt.extend_from_slice(&1_u16.to_le_bytes());
        fmt.extend_from_slice(&rate.to_le_bytes());
        fmt.extend_from_slice(&(rate * 2).to_le_bytes());
        fmt.extend_from_slice(&2_u16.to_le_bytes());
        fmt.extend_from_slice(&16_u16.to_le_bytes());
        push_chunk(&mut bytes, *b"fmt ", &fmt);
        for chunk in chunks {
            push_chunk(&mut bytes, chunk.id, &chunk.body);
        }
        push_chunk(&mut bytes, *b"data", &[0, 0]);
        let riff_size = u32::try_from(bytes.len() - 8).unwrap();
        bytes[4..8].copy_from_slice(&riff_size.to_le_bytes());
        bytes
    }

    fn flac_with_comment(comment: &[u8]) -> Vec<u8> {
        let streaminfo = vec![0_u8; 34];
        let mut bytes = b"fLaC".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 34]);
        bytes.extend_from_slice(&streaminfo);
        let length = u32::try_from(comment.len()).unwrap();
        let header = [
            0x84,
            (length >> 16) as u8,
            (length >> 8) as u8,
            length as u8,
        ];
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(comment);
        bytes
    }

    fn bmff_box(id: [u8; 4], body: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(body.len() + 8);
        bytes.extend_from_slice(&u32::try_from(body.len() + 8).unwrap().to_be_bytes());
        bytes.extend_from_slice(&id);
        bytes.extend_from_slice(body);
        bytes
    }

    fn bmff_full_box(id: [u8; 4], value: &[u8]) -> Vec<u8> {
        let mut body = vec![0_u8; 4];
        body.extend_from_slice(value);
        bmff_box(id, &body)
    }

    fn bmff_data_box(value: &[u8]) -> Vec<u8> {
        let mut body = vec![0_u8; 8];
        body.extend_from_slice(value);
        bmff_box(*b"data", &body)
    }

    fn replaygain_freeform(name: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&bmff_full_box(*b"mean", b"com.apple.iTunes"));
        body.extend_from_slice(&bmff_full_box(*b"name", name));
        body.extend_from_slice(&bmff_full_box(*b"data", b"+1.00 dB"));
        bmff_box(*b"----", &body)
    }

    fn sound_check_freeform() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&bmff_full_box(*b"mean", b"com.apple.iTunes"));
        body.extend_from_slice(&bmff_full_box(*b"name", b"iTunNORM"));
        body.extend_from_slice(&bmff_data_box(
            b" 000003E8 000003E8 000009C4 000009C4 00000000 00000000 00008000 00008000 00000000 00000000",
        ));
        bmff_box(*b"----", &body)
    }

    fn isobmff_with_freeforms(names: &[&[u8]]) -> Vec<u8> {
        let mut ilst_body = Vec::new();
        for name in names {
            ilst_body.extend_from_slice(&replaygain_freeform(name));
        }
        let ilst = bmff_box(*b"ilst", &ilst_body);
        let hdlr = bmff_box(*b"hdlr", b"adapter");
        let mut meta_body = vec![0_u8; 4];
        meta_body.extend_from_slice(&hdlr);
        meta_body.extend_from_slice(&ilst);
        let meta = bmff_box(*b"meta", &meta_body);
        let udta = bmff_box(*b"udta", &meta);
        let moov = bmff_box(*b"moov", &udta);
        let ftyp = bmff_box(*b"ftyp", b"M4A \0\0\0\0M4A ");
        [ftyp, moov].concat()
    }

    fn isobmff_with_sound_check() -> Vec<u8> {
        let ilst = bmff_box(*b"ilst", &sound_check_freeform());
        let hdlr = bmff_box(*b"hdlr", b"adapter");
        let mut meta_body = vec![0_u8; 4];
        meta_body.extend_from_slice(&hdlr);
        meta_body.extend_from_slice(&ilst);
        let meta = bmff_box(*b"meta", &meta_body);
        let udta = bmff_box(*b"udta", &meta);
        let moov = bmff_box(*b"moov", &udta);
        let ftyp = bmff_box(*b"ftyp", b"M4A \0\0\0\0M4A ");
        [ftyp, moov].concat()
    }

    fn expected_bwf_fields(values: [i16; 5]) -> [u8; 12] {
        let mut fields = [0_u8; 12];
        fields[..2].copy_from_slice(&2_u16.to_le_bytes());
        for (index, value) in values.into_iter().enumerate() {
            let start = 2 + index * 2;
            fields[start..start + 2].copy_from_slice(&value.to_le_bytes());
        }
        fields
    }

    #[test]
    fn flac_writer_evidence_accepts_only_well_formed_zero_padding() {
        let padding = |raw: Vec<u8>| MetadataRegion {
            ordinal: 0,
            path: vec!["fLaC".into(), "block:1".into()],
            kind: MetadataKind::FlacBlock { block_type: 1 },
            extents: Vec::new(),
            size: raw.len() as u64,
            raw_sha256: sha256_hex(&raw),
            raw: Some(raw),
            raw_omitted: None,
        };
        assert!(flac_zero_padding_region(&padding(vec![
            0x81, 0, 0, 2, 0, 0
        ])));
        assert!(!flac_zero_padding_region(&padding(vec![
            0x81, 0, 0, 2, 1, 0
        ])));
        assert!(!flac_zero_padding_region(&padding(vec![
            0x81, 0, 0, 3, 0, 0
        ])));
    }

    #[test]
    fn iso_ancestor_evidence_requires_every_direct_child_to_be_bound() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let before_path = directory.path().join("before.m4a");
        let output_path = directory.path().join("output.m4a");
        std::fs::write(&source_path, wave(48_000, &[])).unwrap();
        std::fs::write(&before_path, isobmff_with_freeforms(&[])).unwrap();
        std::fs::write(
            &output_path,
            isobmff_with_freeforms(&[b"replaygain_track_gain"]),
        )
        .unwrap();

        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::IsoBmff,
            48_000,
            48_000,
            0,
            true,
            false,
            &MetadataPolicyConfig::strict(),
        )
        .unwrap();
        let before = prepared.destination_inventory(&before_path).unwrap();
        let after = prepared.destination_inventory(&output_path).unwrap();
        let evidence = prepared.destination_loudness_evidence(
            &before,
            &after,
            &[DestinationLoudnessWriter::ReplayGain],
        );
        let report = prepared
            .finish_with_bwf_loudness_and_evidence(&output_path, None, &evidence)
            .unwrap();
        assert!(report.publication_allowed(), "{report:#?}");

        std::fs::write(
            &output_path,
            isobmff_with_freeforms(&[b"replaygain_track_gain", b"unrelated"]),
        )
        .unwrap();
        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::IsoBmff,
            48_000,
            48_000,
            0,
            true,
            false,
            &MetadataPolicyConfig::strict(),
        )
        .unwrap();
        let after = prepared.destination_inventory(&output_path).unwrap();
        let evidence = prepared.destination_loudness_evidence(
            &before,
            &after,
            &[DestinationLoudnessWriter::ReplayGain],
        );
        let report = prepared
            .finish_with_bwf_loudness_and_evidence(&output_path, None, &evidence)
            .unwrap();
        assert!(!report.publication_allowed(), "{report:#?}");
        assert!(report.entries().iter().any(|entry| {
            entry.loss_class() == MetadataLossClass::Unsupported
                && entry.source_locator().is_none()
                && entry.destination_locator().is_some()
        }));
    }

    #[test]
    fn strict_isobmff_sound_check_writer_evidence_allows_i_tun_norm() {
        assert!(
            !loudness_writers_for_destination(MetadataDestination::IsoBmff, false)
                .contains(&DestinationLoudnessWriter::IsoBmffSoundCheck)
        );

        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let before_path = directory.path().join("before.m4a");
        let output_path = directory.path().join("output.m4a");
        std::fs::write(&source_path, wave(48_000, &[])).unwrap();
        std::fs::write(&before_path, isobmff_with_freeforms(&[])).unwrap();
        std::fs::write(&output_path, isobmff_with_sound_check()).unwrap();

        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::IsoBmff,
            48_000,
            48_000,
            0,
            true,
            false,
            &MetadataPolicyConfig::strict(),
        )
        .unwrap();
        let before = prepared.destination_inventory(&before_path).unwrap();
        let after = prepared.destination_inventory(&output_path).unwrap();
        let evidence = prepared.destination_loudness_evidence(
            &before,
            &after,
            &loudness_writers_for_destination(MetadataDestination::IsoBmff, true),
        );
        assert!(evidence.iter().any(|entry| {
            matches!(
                &entry.kind,
                MetadataKind::IsoBmffBox { id, .. } if id == "----"
            ) && entry.origin == GeneratedDestinationOrigin::LoudnessWriter
        }));

        let report = prepared
            .finish_with_bwf_loudness_and_evidence(&output_path, None, &evidence)
            .unwrap();
        assert!(report.publication_allowed(), "{report:#?}");
        assert!(report
            .entries()
            .iter()
            .all(|entry| { entry.loss_class() == MetadataLossClass::None }));

        std::fs::write(&output_path, isobmff_with_freeforms(&[b"iTunNORM"])).unwrap();
        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::IsoBmff,
            48_000,
            48_000,
            0,
            true,
            false,
            &MetadataPolicyConfig::strict(),
        )
        .unwrap();
        let after = prepared.destination_inventory(&output_path).unwrap();
        let evidence = prepared.destination_loudness_evidence(
            &before,
            &after,
            &[DestinationLoudnessWriter::IsoBmffSoundCheck],
        );
        assert!(!evidence
            .iter()
            .any(|entry| entry.origin == GeneratedDestinationOrigin::LoudnessWriter));
        let report = prepared
            .finish_with_bwf_loudness_and_evidence(&output_path, None, &evidence)
            .unwrap();
        assert!(!report.publication_allowed(), "{report:#?}");
    }

    #[test]
    fn wave_preserve_maps_bext_time_reference_exactly() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let output_path = directory.path().join("output.wav");
        let mut bext = crate::metadata::blank_bext();
        bext[338..346].copy_from_slice(&480_u64.to_le_bytes());
        std::fs::write(
            &source_path,
            wave(
                48_000,
                &[WaveChunk {
                    id: *b"bext",
                    body: bext,
                }],
            ),
        )
        .unwrap();

        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::Wave,
            48_000,
            96_000,
            0,
            true,
            false,
            &MetadataPolicyConfig::preserve(),
        )
        .unwrap();
        let chunks = prepared.wave_chunks().unwrap().to_vec();
        let output_bext = chunks.iter().find(|chunk| chunk.id == *b"bext").unwrap();
        assert_eq!(
            u64::from_le_bytes(output_bext.body[338..346].try_into().unwrap()),
            960
        );
        std::fs::write(&output_path, wave(96_000, &chunks)).unwrap();
        let report = prepared.finish(&output_path).unwrap();
        assert!(report.publication_allowed(), "{report:#?}");
        let entry = report
            .entries()
            .iter()
            .find(|entry| entry.field().contains("TimeReference"))
            .unwrap();
        assert_eq!(entry.outcome(), MetadataOutcome::Mapped);
        assert_eq!(entry.loss_class(), MetadataLossClass::None);
        assert_eq!(
            entry.before_sha256(),
            Some(sha256_hex(&480_u64.to_le_bytes()).as_str())
        );
        assert_eq!(
            entry.after_sha256(),
            Some(sha256_hex(&960_u64.to_le_bytes()).as_str())
        );
    }

    #[test]
    fn strict_rechecks_mapped_timing_bytes_from_the_completed_output() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let output_path = directory.path().join("output.wav");
        let mut bext = crate::metadata::blank_bext();
        bext[338..346].copy_from_slice(&480_u64.to_le_bytes());
        std::fs::write(
            &source_path,
            wave(
                48_000,
                &[WaveChunk {
                    id: *b"bext",
                    body: bext,
                }],
            ),
        )
        .unwrap();
        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::Wave,
            48_000,
            96_000,
            0,
            true,
            false,
            &MetadataPolicyConfig::strict(),
        )
        .unwrap();
        let mut chunks = prepared.wave_chunks().unwrap().to_vec();
        let output_bext = chunks
            .iter_mut()
            .find(|chunk| chunk.id == *b"bext")
            .unwrap();
        output_bext.body[338..346].copy_from_slice(&961_u64.to_le_bytes());
        std::fs::write(&output_path, wave(96_000, &chunks)).unwrap();

        let report = prepared.finish(&output_path).unwrap();
        assert!(!report.publication_allowed());
        let timing = report
            .entries()
            .iter()
            .find(|entry| entry.field().contains("TimeReference"))
            .unwrap();
        assert_eq!(timing.loss_class(), MetadataLossClass::Semantic);
        assert!(timing.reason().contains("completed output bytes"));
        assert_ne!(
            timing.after_sha256(),
            Some(sha256_hex(&960_u64.to_le_bytes()).as_str())
        );
    }

    #[test]
    fn preserve_does_not_promote_an_invalid_unchanged_bext_clock() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let output_path = directory.path().join("output.wav");
        let mut bext = crate::metadata::blank_bext();
        let outside_day = 48_000_u64 * 86_400;
        bext[338..346].copy_from_slice(&outside_day.to_le_bytes());
        std::fs::write(
            &source_path,
            wave(
                48_000,
                &[WaveChunk {
                    id: *b"bext",
                    body: bext,
                }],
            ),
        )
        .unwrap();
        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::Wave,
            48_000,
            48_000,
            0,
            true,
            false,
            &MetadataPolicyConfig::preserve(),
        )
        .unwrap();
        let chunks = prepared.wave_chunks().unwrap().to_vec();
        std::fs::write(&output_path, wave(48_000, &chunks)).unwrap();

        let report = prepared.finish(&output_path).unwrap();
        let entry = report
            .entries()
            .iter()
            .find(|entry| entry.field().contains("TimeReference"))
            .unwrap();
        assert_eq!(entry.outcome(), MetadataOutcome::Mapped);
        assert_eq!(entry.loss_class(), MetadataLossClass::Semantic);
        assert!(entry.reason().contains("24-hour"));
    }

    #[test]
    fn preserve_marks_partial_range_cue_containment_as_unproven() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let output_path = directory.path().join("output.wav");
        let mut cue = vec![0_u8; 28];
        cue[..4].copy_from_slice(&1_u32.to_le_bytes());
        cue[4..8].copy_from_slice(&1_u32.to_le_bytes());
        cue[8..12].copy_from_slice(&1_u32.to_le_bytes());
        cue[12..16].copy_from_slice(b"data");
        cue[24..28].copy_from_slice(&1_u32.to_le_bytes());
        std::fs::write(
            &source_path,
            wave(
                48_000,
                &[WaveChunk {
                    id: *b"cue ",
                    body: cue,
                }],
            ),
        )
        .unwrap();
        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::Wave,
            48_000,
            48_000,
            0,
            false,
            false,
            &MetadataPolicyConfig::preserve(),
        )
        .unwrap();
        let chunks = prepared.wave_chunks().unwrap().to_vec();
        std::fs::write(&output_path, wave(48_000, &chunks)).unwrap();

        let report = prepared.finish(&output_path).unwrap();
        let cue_entries = report
            .entries()
            .iter()
            .filter(|entry| entry.field().starts_with("wave_timing.cue"))
            .collect::<Vec<_>>();
        assert_eq!(cue_entries.len(), 2);
        assert!(cue_entries
            .iter()
            .all(|entry| entry.loss_class() == MetadataLossClass::Semantic));
        assert!(cue_entries
            .iter()
            .all(|entry| entry.reason().contains("containment cannot be proven")));
    }

    #[test]
    fn strict_rejects_unregistered_wave_chunk_before_output_exists() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let output_path = directory.path().join("output.wav");
        std::fs::write(
            &source_path,
            wave(
                48_000,
                &[WaveChunk {
                    id: *b"JUNK",
                    body: b"opaque".to_vec(),
                }],
            ),
        )
        .unwrap();
        let error = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::Wave,
            48_000,
            48_000,
            0,
            true,
            false,
            &MetadataPolicyConfig::strict(),
        )
        .err()
        .unwrap();
        assert!(error.contains("no semantic adapter"));
        assert!(!output_path.exists());
    }

    #[test]
    fn selected_strip_rejects_ambiguous_unselected_timing_metadata() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let mut cue = vec![0_u8; 28];
        cue[..4].copy_from_slice(&1_u32.to_le_bytes());
        cue[4..8].copy_from_slice(&1_u32.to_le_bytes());
        cue[12..16].copy_from_slice(b"slnt");
        std::fs::write(
            &source_path,
            wave(
                48_000,
                &[
                    WaveChunk {
                        id: *b"JUNK",
                        body: b"selected".to_vec(),
                    },
                    WaveChunk {
                        id: *b"cue ",
                        body: cue,
                    },
                ],
            ),
        )
        .unwrap();
        let inventory = discover_path(&source_path, DiscoveryLimits::default()).unwrap();
        let selected = region_locator(inventory.container, &inventory.regions[0]);
        let policy = MetadataPolicyConfig::strip_selected([selected]).unwrap();

        let error = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::Wave,
            48_000,
            44_100,
            0,
            true,
            false,
            &policy,
        )
        .err()
        .unwrap();
        assert!(error.contains("ambiguous"), "unexpected error: {error}");
    }

    #[test]
    fn strict_reports_metadata_moved_across_the_audio_payload() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let output_path = directory.path().join("output.wav");
        let mut source = wave(48_000, &[]);
        let data_start = source
            .windows(4)
            .position(|bytes| bytes == b"data")
            .unwrap();
        let data_body_len =
            u32::from_le_bytes(source[data_start + 4..data_start + 8].try_into().unwrap()) as usize;
        let data_end = data_start + 8 + data_body_len + (data_body_len & 1);
        let mut tail = Vec::new();
        let mut bext = crate::metadata::blank_bext();
        bext[338..346].copy_from_slice(&480_u64.to_le_bytes());
        push_chunk(&mut tail, *b"bext", &bext);
        source.splice(data_end..data_end, tail);
        let riff_size = u32::try_from(source.len() - 8).unwrap();
        source[4..8].copy_from_slice(&riff_size.to_le_bytes());
        std::fs::write(&source_path, source).unwrap();

        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::Wave,
            48_000,
            48_000,
            0,
            true,
            false,
            &MetadataPolicyConfig::strict(),
        )
        .unwrap();
        let chunks = prepared.wave_chunks().unwrap().to_vec();
        std::fs::write(&output_path, wave(48_000, &chunks)).unwrap();
        let report = prepared.finish(&output_path).unwrap();

        assert!(!report.publication_allowed());
        assert!(report.entries().iter().any(|entry| {
            entry.field().starts_with("container_region.")
                && entry.loss_class() == MetadataLossClass::Representation
                && entry.reason().contains("placement relative to audio data")
        }));
    }

    #[test]
    fn strict_detects_nonzero_wave_padding_lost_by_the_writer() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let output_path = directory.path().join("output.wav");
        let mut source = wave(
            48_000,
            &[WaveChunk {
                id: *b"chna",
                body: vec![1, 2, 3],
            }],
        );
        let chunk_start = source
            .windows(4)
            .position(|bytes| bytes == b"chna")
            .unwrap();
        source[chunk_start + 8 + 3] = 0x7f;
        std::fs::write(&source_path, source).unwrap();

        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::Wave,
            48_000,
            48_000,
            0,
            true,
            false,
            &MetadataPolicyConfig::strict(),
        )
        .unwrap();
        let chunks = prepared.wave_chunks().unwrap().to_vec();
        std::fs::write(&output_path, wave(48_000, &chunks)).unwrap();
        let report = prepared.finish(&output_path).unwrap();

        assert!(!report.publication_allowed());
        assert!(report.entries().iter().any(|entry| {
            entry.field().starts_with("container_region.")
                && entry.loss_class() == MetadataLossClass::Representation
                && entry.before_sha256() != entry.after_sha256()
        }));
    }

    #[test]
    fn selected_wave_strip_uses_exact_inventory_locator() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let output_path = directory.path().join("output.wav");
        std::fs::write(
            &source_path,
            wave(
                48_000,
                &[WaveChunk {
                    id: *b"JUNK",
                    body: b"opaque".to_vec(),
                }],
            ),
        )
        .unwrap();
        let inventory = discover_path(&source_path, DiscoveryLimits::default()).unwrap();
        let locator = region_locator(inventory.container, &inventory.regions[0]);
        let policy = MetadataPolicyConfig::strip_selected([locator.clone()]).unwrap();
        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::Wave,
            48_000,
            48_000,
            0,
            true,
            false,
            &policy,
        )
        .unwrap();
        assert!(prepared.wave_chunks().unwrap().is_empty());
        std::fs::write(&output_path, wave(48_000, &[])).unwrap();
        let report = prepared.finish(&output_path).unwrap();
        assert!(report.publication_allowed());
        assert!(report.entries().iter().any(|entry| {
            entry.source_locator() == Some(locator.as_str())
                && entry.outcome() == MetadataOutcome::Dropped
                && entry.loss_class() == MetadataLossClass::Policy
        }));
    }

    #[test]
    fn strip_all_blocks_when_exact_source_metadata_survives() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("source.wav");
        std::fs::write(
            &path,
            wave(
                48_000,
                &[WaveChunk {
                    id: *b"JUNK",
                    body: b"must disappear".to_vec(),
                }],
            ),
        )
        .unwrap();

        let report = evaluate_paths(&path, &path, &MetadataPolicyConfig::strip_all()).unwrap();
        assert!(!report.publication_allowed());
        assert!(report.entries().iter().any(|entry| {
            entry.outcome() == MetadataOutcome::Preserved
                && entry.reason().contains("remain despite")
        }));
    }

    #[test]
    fn strip_does_not_call_a_changed_source_representation_dropped() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let output_path = directory.path().join("output.wav");
        std::fs::write(
            &source_path,
            wave(
                48_000,
                &[WaveChunk {
                    id: *b"JUNK",
                    body: b"source bytes".to_vec(),
                }],
            ),
        )
        .unwrap();
        std::fs::write(
            &output_path,
            wave(
                48_000,
                &[WaveChunk {
                    id: *b"JUNK",
                    body: b"changed bytes".to_vec(),
                }],
            ),
        )
        .unwrap();
        let source = discover_path(&source_path, DiscoveryLimits::default()).unwrap();
        let locator = region_locator(source.container, &source.regions[0]);

        for policy in [
            MetadataPolicyConfig::strip_selected([locator.clone()]).unwrap(),
            MetadataPolicyConfig::strip_all(),
        ] {
            let report = evaluate_paths(&source_path, &output_path, &policy).unwrap();
            assert!(!report.publication_allowed());
            assert!(report.entries().iter().any(|entry| {
                entry.source_locator() == Some(locator.as_str())
                    && entry.outcome() == MetadataOutcome::Mapped
                    && entry.loss_class() == MetadataLossClass::Semantic
                    && entry.before_sha256() != entry.after_sha256()
            }));
            assert!(!report.entries().iter().any(|entry| {
                entry.source_locator() == Some(locator.as_str())
                    && entry.outcome() == MetadataOutcome::Dropped
            }));
        }
    }

    #[test]
    fn strict_blocks_unexplained_destination_only_metadata() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let output_path = directory.path().join("output.wav");
        std::fs::write(&source_path, wave(48_000, &[])).unwrap();
        std::fs::write(
            &output_path,
            wave(
                48_000,
                &[WaveChunk {
                    id: *b"JUNK",
                    body: b"not proven writer output".to_vec(),
                }],
            ),
        )
        .unwrap();

        let report =
            evaluate_paths(&source_path, &output_path, &MetadataPolicyConfig::strict()).unwrap();
        assert!(!report.publication_allowed());
        assert!(report.entries().iter().any(|entry| {
            entry.source_locator().is_none()
                && entry.destination_locator().is_some()
                && entry.outcome() == MetadataOutcome::Recomputed
                && entry.loss_class() == MetadataLossClass::Unsupported
        }));
    }

    #[test]
    fn selected_strip_reserves_an_identical_duplicate_for_the_survivor() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let output_path = directory.path().join("output.wav");
        let duplicate = WaveChunk {
            id: *b"JUNK",
            body: b"duplicate".to_vec(),
        };
        std::fs::write(&source_path, wave(48_000, &[duplicate.clone(), duplicate])).unwrap();
        let inventory = discover_path(&source_path, DiscoveryLimits::default()).unwrap();
        assert_eq!(inventory.regions.len(), 2);
        let selected_locator = region_locator(inventory.container, &inventory.regions[0]);
        let retained_locator = region_locator(inventory.container, &inventory.regions[1]);
        let policy = MetadataPolicyConfig::strip_selected([selected_locator.clone()]).unwrap();
        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::Wave,
            48_000,
            48_000,
            0,
            true,
            false,
            &policy,
        )
        .unwrap();
        let chunks = prepared.wave_chunks().unwrap().to_vec();
        assert_eq!(chunks.len(), 1);
        std::fs::write(&output_path, wave(48_000, &chunks)).unwrap();

        let report = prepared.finish(&output_path).unwrap();
        assert!(report.publication_allowed(), "{report:#?}");
        assert!(report.entries().iter().any(|entry| {
            entry.source_locator() == Some(selected_locator.as_str())
                && entry.outcome() == MetadataOutcome::Dropped
        }));
        assert!(report.entries().iter().any(|entry| {
            entry.source_locator() == Some(retained_locator.as_str())
                && entry.outcome() != MetadataOutcome::Dropped
        }));
    }

    #[test]
    fn selected_strip_does_not_excuse_reordered_survivors() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let output_path = directory.path().join("output.wav");
        let chunks = [
            WaveChunk {
                id: *b"DROP",
                body: b"selected".to_vec(),
            },
            WaveChunk {
                id: *b"ONE ",
                body: b"first survivor".to_vec(),
            },
            WaveChunk {
                id: *b"TWO ",
                body: b"second survivor".to_vec(),
            },
        ];
        std::fs::write(&source_path, wave(48_000, &chunks)).unwrap();
        let inventory = discover_path(&source_path, DiscoveryLimits::default()).unwrap();
        let selected_locator = region_locator(inventory.container, &inventory.regions[0]);
        let policy = MetadataPolicyConfig::strip_selected([selected_locator]).unwrap();
        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::Wave,
            48_000,
            48_000,
            0,
            true,
            false,
            &policy,
        )
        .unwrap();
        let mut output_chunks = prepared.wave_chunks().unwrap().to_vec();
        output_chunks.swap(0, 1);
        std::fs::write(&output_path, wave(48_000, &output_chunks)).unwrap();

        let report = prepared.finish(&output_path).unwrap();
        assert!(!report.publication_allowed());
        assert!(
            report.entries().iter().any(|entry| {
                entry.outcome() == MetadataOutcome::Mapped
                    && entry.loss_class() == MetadataLossClass::Representation
            }),
            "{report:#?}"
        );
    }

    #[test]
    fn exact_matches_are_not_consumed_by_an_earlier_weak_kind_match() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let output_path = directory.path().join("output.wav");
        std::fs::write(
            &source_path,
            wave(
                48_000,
                &[
                    WaveChunk {
                        id: *b"JUNK",
                        body: b"first".to_vec(),
                    },
                    WaveChunk {
                        id: *b"JUNK",
                        body: b"exact survivor".to_vec(),
                    },
                ],
            ),
        )
        .unwrap();
        std::fs::write(
            &output_path,
            wave(
                48_000,
                &[WaveChunk {
                    id: *b"JUNK",
                    body: b"exact survivor".to_vec(),
                }],
            ),
        )
        .unwrap();
        let source = discover_path(&source_path, DiscoveryLimits::default()).unwrap();
        let first_locator = region_locator(source.container, &source.regions[0]);
        let second_locator = region_locator(source.container, &source.regions[1]);

        let report = evaluate_paths(
            &source_path,
            &output_path,
            &MetadataPolicyConfig::preserve(),
        )
        .unwrap();
        assert!(report.entries().iter().any(|entry| {
            entry.source_locator() == Some(first_locator.as_str())
                && entry.outcome() == MetadataOutcome::Dropped
        }));
        assert!(report.entries().iter().any(|entry| {
            entry.source_locator() == Some(second_locator.as_str())
                && entry.outcome() == MetadataOutcome::Mapped
                && entry.before_sha256() == entry.after_sha256()
        }));
    }

    #[test]
    fn requested_bwf_expands_a_short_legacy_bext_before_rendering() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let output_path = directory.path().join("output.wav");
        let mut legacy_bext = vec![0_u8; 350];
        legacy_bext[338..346].copy_from_slice(&480_u64.to_le_bytes());
        legacy_bext[346..348].copy_from_slice(&1_u16.to_le_bytes());
        std::fs::write(
            &source_path,
            wave(
                48_000,
                &[WaveChunk {
                    id: *b"bext",
                    body: legacy_bext,
                }],
            ),
        )
        .unwrap();

        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::Wave,
            48_000,
            48_000,
            0,
            true,
            true,
            &MetadataPolicyConfig::preserve(),
        )
        .unwrap();
        let mut chunks = prepared.wave_chunks().unwrap().to_vec();
        let bext = chunks
            .iter_mut()
            .find(|chunk| chunk.id == *b"bext")
            .unwrap();
        assert_eq!(bext.body.len(), 602);
        assert_eq!(
            u16::from_le_bytes(bext.body[346..348].try_into().unwrap()),
            2
        );
        for (index, offset) in [412_usize, 414, 416, 418, 420].into_iter().enumerate() {
            bext.body[offset..offset + 2]
                .copy_from_slice(&i16::try_from(index + 1).unwrap().to_le_bytes());
        }
        std::fs::write(&output_path, wave(48_000, &chunks)).unwrap();

        let report = prepared
            .finish_with_bwf_loudness(&output_path, Some(expected_bwf_fields([1, 2, 3, 4, 5])))
            .unwrap();
        assert!(report.publication_allowed());
        let recomputed = report
            .entries()
            .iter()
            .filter(|entry| entry.field().starts_with("wave_bwf."))
            .collect::<Vec<_>>();
        assert_eq!(recomputed.len(), 6);
        assert!(recomputed
            .iter()
            .all(|entry| entry.outcome() == MetadataOutcome::Recomputed));
        let version = recomputed
            .iter()
            .find(|entry| entry.field() == "wave_bwf.Version")
            .unwrap();
        assert_eq!(
            version.before_sha256(),
            Some(sha256_hex(&1_u16.to_le_bytes()).as_str())
        );
        assert_eq!(
            version.after_sha256(),
            Some(sha256_hex(&2_u16.to_le_bytes()).as_str())
        );
    }

    #[test]
    fn generated_bwf_is_the_only_trusted_destination_only_wave_region() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let output_path = directory.path().join("output.wav");
        std::fs::write(&source_path, wave(48_000, &[])).unwrap();
        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::Wave,
            48_000,
            48_000,
            0,
            true,
            true,
            &MetadataPolicyConfig::strict(),
        )
        .unwrap();
        let mut chunks = prepared.wave_chunks().unwrap().to_vec();
        let bext = chunks
            .iter_mut()
            .find(|chunk| chunk.id == *b"bext")
            .unwrap();
        bext.body[412..414].copy_from_slice(&(-2300_i16).to_le_bytes());
        std::fs::write(&output_path, wave(48_000, &chunks)).unwrap();

        let report = prepared
            .finish_with_bwf_loudness(&output_path, Some(expected_bwf_fields([-2300, 0, 0, 0, 0])))
            .unwrap();
        assert!(report.publication_allowed());
        assert_eq!(
            report
                .entries()
                .iter()
                .filter(|entry| entry.field().starts_with("wave_bwf."))
                .count(),
            6
        );
        assert!(report.entries().iter().all(|entry| {
            entry.loss_class() == MetadataLossClass::None
                || !entry.field().starts_with("container_region.")
        }));
    }

    #[test]
    fn strip_all_reserves_generated_bwf_before_source_drop_matching() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let output_path = directory.path().join("output.wav");
        std::fs::write(
            &source_path,
            wave(
                48_000,
                &[WaveChunk {
                    id: *b"bext",
                    body: crate::metadata::blank_bext(),
                }],
            ),
        )
        .unwrap();
        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::Wave,
            48_000,
            48_000,
            0,
            true,
            true,
            &MetadataPolicyConfig::strip_all(),
        )
        .unwrap();
        let expected = expected_bwf_fields([-2300, 0, 0, 0, 0]);
        let mut chunks = prepared.wave_chunks().unwrap().to_vec();
        let generated = chunks
            .iter_mut()
            .find(|chunk| chunk.id == *b"bext")
            .unwrap();
        generated.body[412..422].copy_from_slice(&expected[2..]);
        std::fs::write(&output_path, wave(48_000, &chunks)).unwrap();

        let report = prepared
            .finish_with_bwf_loudness(&output_path, Some(expected))
            .unwrap();
        assert!(report.publication_allowed(), "{report:#?}");
        let source_bext = report
            .entries()
            .iter()
            .find(|entry| {
                entry.source_locator().is_some()
                    && entry.field()
                        == "container_region.{\"kind\":\"wave_chunk\",\"id\":\"bext\",\"audio_data\":false}"
            })
            .expect("source bext ledger entry");
        assert_eq!(source_bext.outcome(), MetadataOutcome::Dropped);
        assert!(report.entries().iter().any(|entry| {
            entry.field().starts_with("wave_bwf.")
                && entry.source_locator().is_none()
                && entry.outcome() == MetadataOutcome::Recomputed
                && entry.loss_class() == MetadataLossClass::None
        }));
    }

    #[test]
    fn destination_loudness_evidence_requires_exact_output_bytes() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let output_path = directory.path().join("output.flac");
        std::fs::write(&source_path, wave(48_000, &[])).unwrap();
        std::fs::write(&output_path, flac_with_comment(b"before")).unwrap();
        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::Flac,
            48_000,
            48_000,
            0,
            true,
            false,
            &MetadataPolicyConfig::strict(),
        )
        .unwrap();
        let before = prepared.destination_inventory(&output_path).unwrap();
        std::fs::write(&output_path, flac_with_comment(b"after")).unwrap();
        let after = prepared.destination_inventory(&output_path).unwrap();
        let evidence = prepared.destination_loudness_evidence(
            &before,
            &after,
            &[DestinationLoudnessWriter::ReplayGain],
        );
        assert_eq!(evidence.len(), 1);
        let report = prepared
            .finish_with_bwf_loudness_and_evidence(&output_path, None, &evidence)
            .unwrap();
        assert!(report.publication_allowed(), "{report:#?}");

        std::fs::write(&output_path, flac_with_comment(b"tampered")).unwrap();
        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::Flac,
            48_000,
            48_000,
            0,
            true,
            false,
            &MetadataPolicyConfig::strict(),
        )
        .unwrap();
        let report = prepared
            .finish_with_bwf_loudness_and_evidence(&output_path, None, &evidence)
            .unwrap();
        assert!(!report.publication_allowed(), "{report:#?}");
        assert!(report.entries().iter().any(|entry| {
            entry.outcome() == MetadataOutcome::Recomputed
                && entry.loss_class() == MetadataLossClass::Unsupported
        }));
    }

    #[test]
    fn changed_existing_composite_region_is_not_wholly_blessed_by_writer_evidence() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.flac");
        let output_path = directory.path().join("output.flac");
        std::fs::write(&source_path, flac_with_comment(b"old replaygain")).unwrap();
        std::fs::write(&output_path, flac_with_comment(b"new replaygain")).unwrap();
        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::Flac,
            48_000,
            48_000,
            0,
            true,
            false,
            &MetadataPolicyConfig::preserve(),
        )
        .unwrap();
        let before = prepared.destination_inventory(&output_path).unwrap();
        std::fs::write(&output_path, flac_with_comment(b"writer replaygain")).unwrap();
        let after = prepared.destination_inventory(&output_path).unwrap();
        let evidence = prepared.destination_loudness_evidence(
            &before,
            &after,
            &[DestinationLoudnessWriter::ReplayGain],
        );
        let report = prepared
            .finish_with_bwf_loudness_and_evidence(&output_path, None, &evidence)
            .unwrap();
        assert!(report.publication_allowed(), "{report:#?}");
        let region = report
            .entries()
            .iter()
            .find(|entry| entry.field().contains("flac_block"))
            .expect("source FLAC metadata region ledger entry");
        assert_eq!(region.outcome(), MetadataOutcome::Mapped);
        assert_eq!(region.loss_class(), MetadataLossClass::Representation);
    }

    #[test]
    fn strict_bwf_generation_requires_the_authoritative_measured_fields() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let output_path = directory.path().join("output.wav");
        std::fs::write(&source_path, wave(48_000, &[])).unwrap();
        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::Wave,
            48_000,
            48_000,
            0,
            true,
            true,
            &MetadataPolicyConfig::strict(),
        )
        .unwrap();
        let mut chunks = prepared.wave_chunks().unwrap().to_vec();
        let bext = chunks
            .iter_mut()
            .find(|chunk| chunk.id == *b"bext")
            .unwrap();
        bext.body[412..414].copy_from_slice(&(-2200_i16).to_le_bytes());
        std::fs::write(&output_path, wave(48_000, &chunks)).unwrap();

        let report = prepared
            .finish_with_bwf_loudness(&output_path, Some(expected_bwf_fields([-2300, 0, 0, 0, 0])))
            .unwrap();
        assert!(!report.publication_allowed());
        assert!(report.entries().iter().any(|entry| {
            entry.source_locator().is_none()
                && entry.outcome() == MetadataOutcome::Recomputed
                && entry.loss_class() == MetadataLossClass::Unsupported
        }));
    }

    #[test]
    fn duplicate_bext_only_attributes_recomputed_fields_to_the_first_chunk() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let output_path = directory.path().join("output.wav");
        let first = crate::metadata::blank_bext();
        let mut second = crate::metadata::blank_bext();
        second[0] = 1;
        std::fs::write(
            &source_path,
            wave(
                48_000,
                &[
                    WaveChunk {
                        id: *b"bext",
                        body: first,
                    },
                    WaveChunk {
                        id: *b"bext",
                        body: second,
                    },
                ],
            ),
        )
        .unwrap();
        let prepared = PreparedMetadata::prepare(
            &source_path,
            MetadataDestination::Wave,
            48_000,
            48_000,
            0,
            true,
            true,
            &MetadataPolicyConfig::preserve(),
        )
        .unwrap();
        let mut chunks = prepared.wave_chunks().unwrap().to_vec();
        chunks[0].body[412..414].copy_from_slice(&(-2300_i16).to_le_bytes());
        std::fs::write(&output_path, wave(48_000, &chunks)).unwrap();

        let report = prepared
            .finish_with_bwf_loudness(&output_path, Some(expected_bwf_fields([-2300, 0, 0, 0, 0])))
            .unwrap();
        assert_eq!(
            report
                .entries()
                .iter()
                .filter(|entry| entry.field().starts_with("wave_bwf."))
                .count(),
            6
        );
    }

    #[test]
    fn region_locators_preserve_path_boundaries_and_stay_bounded() {
        let region = |path: Vec<String>| MetadataRegion {
            ordinal: 7,
            path,
            kind: MetadataKind::Apev2Item { key: "key".into() },
            extents: Vec::new(),
            size: 0,
            raw_sha256: sha256_hex(&[]),
            raw: None,
            raw_omitted: None,
        };
        let first = region_locator(ContainerKind::Mp3, &region(vec!["a/b".into(), "c".into()]));
        let second = region_locator(ContainerKind::Mp3, &region(vec!["a".into(), "b/c".into()]));
        assert_ne!(first, second);
        assert!(first.contains("[\"a/b\",\"c\"]"));

        let long = region_locator(
            ContainerKind::Mp3,
            &region(vec!["x".repeat(1024), "y".repeat(1024)]),
        );
        assert!(long.len() <= 1024);
        assert!(long.contains("path-sha256:"));
        assert!(long.ends_with("/#7"));
    }

    #[test]
    fn long_container_identifier_has_a_bounded_stable_field_id() {
        let region = MetadataRegion {
            ordinal: 0,
            path: vec!["MPEG".into(), "APEv2".into(), "x".repeat(255)],
            kind: MetadataKind::Apev2Item {
                key: "x".repeat(255),
            },
            extents: Vec::new(),
            size: 0,
            raw_sha256: sha256_hex(&[]),
            raw: None,
            raw_omitted: None,
        };
        let field = region_field(&region);
        assert!(field.len() <= 256);
        assert_eq!(field, region_field(&region));
        assert!(field.starts_with("container_region.sha256."));
    }
}
