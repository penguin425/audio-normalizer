//! Content and request binding for reusable loudness measurements.
//!
//! [`Analysis`] remains the compatibility type for
//! measured values. [`BoundAnalysis`] is the additive safe type used when a
//! measurement is reused for rendering: it proves which encoded bytes and
//! measurement-domain request produced those values.

use crate::analysis::Analysis;
use crate::decoder::{
    ChannelLayoutProvenance, InputDescriptor, InputDescriptorOptions, SourceFrameRange,
};
use crate::dsp::resample::ResampleQuality;
use crate::normalize::Plan;
use crate::stable_input::{InputContentBinding, StableInput};
use crate::wav::ChannelRole;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fmt;

/// Version of the in-process bound-analysis contract.
pub const BOUND_ANALYSIS_VERSION: u32 = 2;

/// Revision of the measurement implementation represented by bound results.
pub const MEASUREMENT_ALGORITHM_REVISION: &str = "forge-bs1770-5-r4";

/// Stable classification for bound-analysis failures.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundAnalysisErrorKind {
    /// A plan or measurement request is invalid.
    InvalidRequest,
    /// Analysis of the stable snapshot failed.
    AnalysisFailed,
    /// The supplied stable input contains different encoded bytes.
    InputContentMismatch,
    /// The decoder, layout, range, or resampling request does not match.
    AnalysisRequestMismatch,
    /// The analysis was produced by another measurement revision.
    UnsupportedAlgorithmRevision,
    /// Rendering or publication failed after binding validation.
    RenderFailed,
}

/// Error returned by bound analysis and rendering APIs.
#[derive(Clone, Debug)]
pub struct BoundAnalysisError {
    kind: BoundAnalysisErrorKind,
    message: String,
}

impl BoundAnalysisError {
    pub(crate) fn new(kind: BoundAnalysisErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(BoundAnalysisErrorKind::InvalidRequest, message)
    }

    pub(crate) fn analysis_failed(message: impl Into<String>) -> Self {
        Self::new(BoundAnalysisErrorKind::AnalysisFailed, message)
    }

    pub(crate) fn render_failed(message: impl Into<String>) -> Self {
        Self::new(BoundAnalysisErrorKind::RenderFailed, message)
    }

    /// Machine-readable failure classification.
    pub const fn kind(&self) -> BoundAnalysisErrorKind {
        self.kind
    }
}

impl fmt::Display for BoundAnalysisError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for BoundAnalysisError {}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OutputDomainRequest {
    decoder_route: String,
    source_range: SourceFrameRange,
    declared_layout_provenance: ChannelLayoutProvenance,
    effective_roles: Vec<ChannelRole>,
    explicit_roles: bool,
    output_sample_rate: Option<u32>,
    resample_quality: ResampleQuality,
}

/// An analysis bound to encoded content and every measurement-domain option.
///
/// Fields are private so future versions can extend the evidence without
/// changing the established [`Analysis`] structure.
#[derive(Clone, Debug)]
pub struct BoundAnalysis {
    analysis: Analysis,
    input_binding: InputContentBinding,
    request: OutputDomainRequest,
    request_sha256: [u8; 32],
    algorithm_revision: &'static str,
}

impl BoundAnalysis {
    pub(crate) fn for_output_domain(
        input: &StableInput,
        analysis: Analysis,
        requested_roles: Option<&[ChannelRole]>,
        plan: &Plan,
    ) -> Result<Self, BoundAnalysisError> {
        let mut options = InputDescriptorOptions::default();
        if let Some(roles) = requested_roles {
            options = options.with_channel_roles(roles.to_vec());
        }
        let descriptor = InputDescriptor::probe(input.clone(), options)
            .map_err(BoundAnalysisError::analysis_failed)?;
        Self::for_descriptor(&descriptor, analysis, plan)
    }

    pub(crate) fn for_descriptor(
        descriptor: &InputDescriptor,
        analysis: Analysis,
        plan: &Plan,
    ) -> Result<Self, BoundAnalysisError> {
        plan.validate()
            .map_err(BoundAnalysisError::invalid_request)?;
        if analysis.channel_roles.len() != usize::from(analysis.channels) {
            return Err(BoundAnalysisError::analysis_failed(
                "analysis channel-role count does not match its channel count",
            ));
        }
        if descriptor.stream_info().channel_roles != analysis.channel_roles {
            return Err(BoundAnalysisError::analysis_failed(
                "effective analysis roles do not match the input descriptor",
            ));
        }
        let expected_sample_rate = plan
            .output_sample_rate
            .unwrap_or(descriptor.stream_info().sample_rate);
        if analysis.sample_rate != expected_sample_rate {
            return Err(BoundAnalysisError::analysis_failed(
                "analysis sample rate does not match the requested output domain",
            ));
        }
        if analysis.channels != descriptor.stream_info().channels {
            return Err(BoundAnalysisError::analysis_failed(
                "analysis channel count does not match the input descriptor",
            ));
        }
        if analysis.kind != descriptor.stream_info().source_kind {
            return Err(BoundAnalysisError::analysis_failed(
                "analysis PCM kind does not match the input descriptor",
            ));
        }
        if analysis.frames == 0 {
            return Err(BoundAnalysisError::analysis_failed(
                "bound analysis must contain at least one decoded frame",
            ));
        }
        let request = OutputDomainRequest {
            decoder_route: descriptor.decoder_route_id(),
            source_range: descriptor.source_range(),
            declared_layout_provenance: descriptor.declared_layout_provenance(),
            effective_roles: analysis.channel_roles.clone(),
            explicit_roles: descriptor.uses_explicit_channel_roles(),
            output_sample_rate: plan.output_sample_rate,
            resample_quality: plan.resample_quality,
        };
        let request_sha256 = request_digest(&request);
        Ok(Self {
            analysis,
            input_binding: descriptor.stable_input().binding().clone(),
            request,
            request_sha256,
            algorithm_revision: MEASUREMENT_ALGORITHM_REVISION,
        })
    }

    /// Bound measured values.
    pub fn analysis(&self) -> &Analysis {
        &self.analysis
    }

    /// In-process bound-analysis contract version.
    pub const fn version(&self) -> u32 {
        BOUND_ANALYSIS_VERSION
    }

    /// Exact encoded-content identity used by the measurement.
    pub fn input_binding(&self) -> &InputContentBinding {
        &self.input_binding
    }

    /// Canonical SHA-256 of the measurement-domain request.
    pub const fn request_sha256(&self) -> &[u8; 32] {
        &self.request_sha256
    }

    /// Lower-case hexadecimal canonical request digest.
    pub fn request_sha256_hex(&self) -> String {
        hex_digest(&self.request_sha256)
    }

    /// Measurement implementation revision used to produce this result.
    pub const fn algorithm_revision(&self) -> &str {
        self.algorithm_revision
    }

    /// Validate content and measurement-domain compatibility before render.
    pub fn validate_for_plan(
        &self,
        input: &StableInput,
        plan: &Plan,
    ) -> Result<(), BoundAnalysisError> {
        input.verify_source().map_err(|error| {
            BoundAnalysisError::new(
                BoundAnalysisErrorKind::AnalysisRequestMismatch,
                error.to_string(),
            )
        })?;
        let mut options = InputDescriptorOptions::default();
        if self.request.explicit_roles {
            options = options.with_channel_roles(self.request.effective_roles.clone());
        }
        let descriptor = InputDescriptor::probe(input.clone(), options).map_err(|error| {
            BoundAnalysisError::new(BoundAnalysisErrorKind::AnalysisRequestMismatch, error)
        })?;
        self.validate_descriptor_for_plan(&descriptor, plan)
    }

    pub(crate) fn validate_descriptor_for_plan(
        &self,
        descriptor: &InputDescriptor,
        plan: &Plan,
    ) -> Result<(), BoundAnalysisError> {
        plan.validate()
            .map_err(BoundAnalysisError::invalid_request)?;
        if self.algorithm_revision != MEASUREMENT_ALGORITHM_REVISION {
            return Err(BoundAnalysisError::new(
                BoundAnalysisErrorKind::UnsupportedAlgorithmRevision,
                "bound analysis measurement revision is unsupported",
            ));
        }
        if self.input_binding != *descriptor.stable_input().binding() {
            return Err(BoundAnalysisError::new(
                BoundAnalysisErrorKind::InputContentMismatch,
                "bound analysis does not describe the supplied input bytes",
            ));
        }
        let expected = OutputDomainRequest {
            decoder_route: descriptor.decoder_route_id(),
            source_range: descriptor.source_range(),
            declared_layout_provenance: descriptor.declared_layout_provenance(),
            effective_roles: self.request.effective_roles.clone(),
            explicit_roles: descriptor.uses_explicit_channel_roles(),
            output_sample_rate: plan.output_sample_rate,
            resample_quality: plan.resample_quality,
        };
        if expected != self.request
            || request_digest(&expected) != self.request_sha256
            || self.analysis.channel_roles != self.request.effective_roles
            || plan
                .output_sample_rate
                .is_some_and(|sample_rate| sample_rate != self.analysis.sample_rate)
        {
            return Err(BoundAnalysisError::new(
                BoundAnalysisErrorKind::AnalysisRequestMismatch,
                "bound analysis request does not match the supplied input and plan",
            ));
        }
        Ok(())
    }

    pub(crate) fn used_explicit_roles(&self) -> bool {
        self.request.explicit_roles
    }

    pub(crate) fn explicit_roles(&self) -> Option<&[ChannelRole]> {
        self.request
            .explicit_roles
            .then_some(self.request.effective_roles.as_slice())
    }
}

fn request_digest(request: &OutputDomainRequest) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"forge-bound-analysis-request-v2\0output-domain\0");
    update_len_prefixed(&mut digest, request.decoder_route.as_bytes());
    digest.update(request.source_range.start().to_le_bytes());
    match request.source_range.frames() {
        Some(frames) => {
            digest.update([1]);
            digest.update(frames.to_le_bytes());
        }
        None => digest.update([0]),
    }
    digest.update([match request.declared_layout_provenance {
        ChannelLayoutProvenance::KnownSpeakers => 0,
        ChannelLayoutProvenance::Unknown => 1,
        ChannelLayoutProvenance::SceneBased => 2,
    }]);
    digest.update([u8::from(request.explicit_roles)]);
    digest.update((request.effective_roles.len() as u64).to_le_bytes());
    for role in &request.effective_roles {
        match *role {
            ChannelRole::Main => digest.update([0]),
            ChannelRole::Surround => digest.update([1]),
            ChannelRole::DualMono => digest.update([2]),
            ChannelRole::Positioned {
                azimuth_degrees,
                elevation_degrees,
            } => {
                digest.update([3]);
                digest.update(azimuth_degrees.to_le_bytes());
                digest.update(elevation_degrees.to_le_bytes());
            }
            ChannelRole::Lfe => digest.update([4]),
        }
    }
    match request.output_sample_rate {
        Some(sample_rate) => {
            digest.update([1]);
            digest.update(sample_rate.to_le_bytes());
        }
        None => digest.update([0]),
    }
    digest.update([match request.resample_quality {
        ResampleQuality::Fast => 0,
        ResampleQuality::Balanced => 1,
        ResampleQuality::Best => 2,
    }]);
    update_len_prefixed(&mut digest, MEASUREMENT_ALGORITHM_REVISION.as_bytes());
    digest.finalize().into()
}

fn update_len_prefixed(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_le_bytes());
    digest.update(bytes);
}

fn hex_digest(digest: &[u8; 32]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(64);
    for byte in digest {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::limiter::LimiterConfig;
    use crate::normalize::Mode;
    use crate::stable_input::StableInputOptions;
    use crate::wav::{AudioBuffer, PcmKind, WavContainer, WavWriter};

    fn plan() -> Plan {
        Plan {
            mode: Mode::Lufs,
            target_lufs: -16.0,
            target_peak_db: -1.0,
            target_rms_db: -18.0,
            ceiling_db: -1.0,
            max_gain_db: None,
            dither: false,
            output_kind: None,
            mp3_bitrate: 192,
            mp3_quality: 2,
            limiter: None::<LimiterConfig>,
            wav_container: WavContainer::Auto,
            bwf: false,
            output_sample_rate: None,
            resample_quality: ResampleQuality::Balanced,
        }
    }

    fn analysis(roles: Vec<ChannelRole>) -> Analysis {
        Analysis {
            sample_rate: 48_000,
            channels: roles.len() as u16,
            channel_roles: roles,
            frames: 48_000,
            kind: PcmKind::F32,
            lufs: -20.0,
            max_momentary_lufs: -19.0,
            max_short_term_lufs: -19.5,
            loudness_range_lu: 1.0,
            rms_db: -20.0,
            sample_peak: 0.1,
            true_peak: 0.11,
            loudness_blocks: vec![0.01],
        }
    }

    fn input(amplitude: f32, hint: &str) -> StableInput {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fixture.wav");
        WavWriter::write(
            &path,
            &AudioBuffer {
                sample_rate: 48_000,
                channels: 1,
                channel_roles: vec![ChannelRole::Main],
                frames: 48_000,
                data: vec![vec![amplitude; 48_000]],
                source_kind: PcmKind::F32,
            },
            PcmKind::F32,
            false,
        )
        .unwrap();
        let bytes = std::fs::read(path).unwrap();
        let options = StableInputOptions::new(bytes.len() as u64)
            .unwrap()
            .with_source_name_hint(hint);
        StableInput::from_bytes(&bytes, &options).unwrap()
    }

    #[test]
    fn validates_matching_content_and_measurement_request() {
        let input = input(0.1, "track.wav");
        let roles = vec![ChannelRole::Main];
        let bound = BoundAnalysis::for_output_domain(
            &input,
            analysis(roles.clone()),
            Some(&roles),
            &plan(),
        )
        .unwrap();
        bound.validate_for_plan(&input, &plan()).unwrap();
        assert_eq!(bound.analysis().channel_roles, roles);
        assert_eq!(bound.explicit_roles(), Some(roles.as_slice()));
        assert_eq!(bound.request_sha256_hex().len(), 64);
    }

    #[test]
    fn rejects_content_route_rate_and_quality_mismatches() {
        let original_input = input(0.1, "track.wav");
        let roles = vec![ChannelRole::Main];
        let bound = BoundAnalysis::for_output_domain(
            &original_input,
            analysis(roles.clone()),
            Some(&roles),
            &plan(),
        )
        .unwrap();

        let changed = input(0.2, "track.wav");
        assert_eq!(
            bound
                .validate_for_plan(&changed, &plan())
                .unwrap_err()
                .kind(),
            BoundAnalysisErrorKind::InputContentMismatch
        );
        let misleading_suffix = input(0.1, "track.flac");
        bound
            .validate_for_plan(&misleading_suffix, &plan())
            .unwrap();
        let mut changed_plan = plan();
        changed_plan.output_sample_rate = Some(48_000);
        assert_eq!(
            bound
                .validate_for_plan(&original_input, &changed_plan)
                .unwrap_err()
                .kind(),
            BoundAnalysisErrorKind::AnalysisRequestMismatch
        );
        changed_plan = plan();
        changed_plan.resample_quality = ResampleQuality::Best;
        assert_eq!(
            bound
                .validate_for_plan(&original_input, &changed_plan)
                .unwrap_err()
                .kind(),
            BoundAnalysisErrorKind::AnalysisRequestMismatch
        );
    }
}
