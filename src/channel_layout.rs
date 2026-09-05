//! Exact, additive channel-layout evidence shared by decode and measurement.
//!
//! The long-standing [`crate::wav::ChannelRole`] type remains the compact
//! BS.1770 compatibility view. This module keeps the information that view
//! cannot express: standardized speaker identities, partial assignments,
//! scene/object channels, raw container declarations, and renderer bindings.

use crate::wav::{ChannelLayoutProvenance, ChannelRole};
use serde::{Deserialize, Serialize};

/// Version of the serialized exact channel-layout descriptor.
pub const CHANNEL_LAYOUT_DESCRIPTOR_VERSION: u32 = 1;
/// Maximum accepted size of one foreign-API descriptor JSON document.
///
/// The bound is large enough for the maximum `u16` channel population in the
/// compact canonical encoding while still rejecting unbounded whitespace or
/// otherwise pathological inputs before deserialization.
pub const MAX_CHANNEL_LAYOUT_JSON_BYTES: usize = 16 * 1024 * 1024;

/// Semantic kind of one decoded PCM plane.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChannelAssignmentKind {
    /// A physical loudspeaker with a known nominal position.
    Speaker,
    /// A low-frequency-effects channel, excluded from BS.1770 summation.
    LowFrequencyEffects,
    /// A compatibility role explicitly supplied through an older API.
    LegacyRole,
    /// The source does not assign this plane to a loudspeaker.
    Unassigned,
    /// One Ambisonic component identified by ACN or source order.
    Ambisonic,
    /// One object signal that requires rendering before measurement.
    Object,
}

/// Exact assignment of one decoded PCM plane.
///
/// Fields are private so the representation can grow without changing the
/// stable Rust API. Construct values through the checked constructors and use
/// the accessors when exporting evidence.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelAssignment {
    kind: ChannelAssignmentKind,
    role: ChannelRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    cicp_position: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    azimuth_degrees: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    elevation_degrees: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    component_index: Option<u32>,
}

impl ChannelAssignment {
    /// Construct a standardized CICP output-channel position.
    ///
    /// Reserved codes and the CICP unknown/undefined sentinel are rejected;
    /// callers represent an unknown decoded plane with [`Self::unassigned`].
    pub fn from_cicp_position(position: u8) -> Result<Self, String> {
        validate_cicp_position(position)?;
        let value = Self::cicp(position);
        value.validate()?;
        Ok(value)
    }

    /// Construct a physical speaker at an explicit integral-degree position.
    pub fn speaker(azimuth_degrees: i16, elevation_degrees: i16) -> Result<Self, String> {
        validate_angles(azimuth_degrees, elevation_degrees)?;
        Ok(Self {
            kind: ChannelAssignmentKind::Speaker,
            role: ChannelRole::positioned(azimuth_degrees, elevation_degrees),
            cicp_position: None,
            azimuth_degrees: Some(azimuth_degrees),
            elevation_degrees: Some(elevation_degrees),
            component_index: None,
        })
    }

    /// Construct a low-frequency-effects plane.
    pub const fn low_frequency_effects() -> Self {
        Self {
            kind: ChannelAssignmentKind::LowFrequencyEffects,
            role: ChannelRole::Lfe,
            cicp_position: None,
            azimuth_degrees: None,
            elevation_degrees: None,
            component_index: None,
        }
    }

    /// Construct a plane whose physical assignment is not declared.
    pub const fn unassigned(channel_index: u32) -> Self {
        Self {
            kind: ChannelAssignmentKind::Unassigned,
            role: ChannelRole::Main,
            cicp_position: None,
            azimuth_degrees: None,
            elevation_degrees: None,
            component_index: Some(channel_index),
        }
    }

    /// Construct an Ambisonic component identified by ACN/source order.
    pub const fn ambisonic(component_index: u32) -> Self {
        Self {
            kind: ChannelAssignmentKind::Ambisonic,
            role: ChannelRole::Main,
            cicp_position: None,
            azimuth_degrees: None,
            elevation_degrees: None,
            component_index: Some(component_index),
        }
    }

    /// Construct an object signal that must be rendered before measurement.
    pub const fn object(object_index: u32) -> Self {
        Self {
            kind: ChannelAssignmentKind::Object,
            role: ChannelRole::Main,
            cicp_position: None,
            azimuth_degrees: None,
            elevation_degrees: None,
            component_index: Some(object_index),
        }
    }

    /// Construct the exact compatibility meaning of a legacy channel role.
    pub const fn legacy_role(role: ChannelRole) -> Self {
        let (azimuth_degrees, elevation_degrees) = match role {
            ChannelRole::Positioned {
                azimuth_degrees,
                elevation_degrees,
            } => (Some(azimuth_degrees), Some(elevation_degrees)),
            _ => (None, None),
        };
        Self {
            kind: ChannelAssignmentKind::LegacyRole,
            role,
            cicp_position: None,
            azimuth_degrees,
            elevation_degrees,
            component_index: None,
        }
    }

    pub(crate) fn cicp(position: u8) -> Self {
        let (role, azimuth_degrees, elevation_degrees) = cicp_role(position);
        let kind = if role == ChannelRole::Lfe {
            ChannelAssignmentKind::LowFrequencyEffects
        } else if azimuth_degrees.is_some() {
            ChannelAssignmentKind::Speaker
        } else {
            ChannelAssignmentKind::Unassigned
        };
        Self {
            kind,
            role,
            cicp_position: Some(position),
            azimuth_degrees,
            elevation_degrees,
            component_index: None,
        }
    }

    pub(crate) fn explicit_cicp_speaker(
        azimuth_degrees: i16,
        elevation_degrees: i16,
    ) -> Result<Self, String> {
        let mut value = Self::speaker(azimuth_degrees, elevation_degrees)?;
        value.cicp_position = Some(126);
        Ok(value)
    }

    /// Semantic assignment kind.
    pub const fn kind(&self) -> ChannelAssignmentKind {
        self.kind
    }

    /// CICP `OutputChannelPosition`, when one was explicitly signalled.
    pub const fn cicp_position(&self) -> Option<u8> {
        self.cicp_position
    }

    /// Nominal or explicitly signalled speaker azimuth in degrees.
    pub const fn azimuth_degrees(&self) -> Option<i16> {
        self.azimuth_degrees
    }

    /// Nominal or explicitly signalled speaker elevation in degrees.
    pub const fn elevation_degrees(&self) -> Option<i16> {
        self.elevation_degrees
    }

    /// Ambisonic component, object, or unassigned-plane index.
    pub const fn component_index(&self) -> Option<u32> {
        self.component_index
    }

    /// Compatibility view consumed by the established BS.1770 engine.
    pub const fn channel_role(&self) -> ChannelRole {
        self.role
    }

    fn with_compatibility_role(mut self, role: ChannelRole) -> Self {
        self.role = role;
        match role {
            ChannelRole::Positioned {
                azimuth_degrees,
                elevation_degrees,
            } => {
                self.azimuth_degrees = Some(azimuth_degrees);
                self.elevation_degrees = Some(elevation_degrees);
            }
            ChannelRole::Lfe => {
                self.kind = ChannelAssignmentKind::LowFrequencyEffects;
                self.azimuth_degrees = None;
                self.elevation_degrees = None;
            }
            _ => {}
        }
        self
    }

    fn validate(&self) -> Result<(), String> {
        if let Some(position) = self.cicp_position {
            validate_cicp_position(position)?;
            if position == 126 {
                if self.kind != ChannelAssignmentKind::Speaker {
                    return Err("explicit CICP speaker fields are inconsistent".into());
                }
            } else {
                let canonical = Self::cicp(position);
                if self.kind != canonical.kind
                    || self.azimuth_degrees != canonical.azimuth_degrees
                    || self.elevation_degrees != canonical.elevation_degrees
                {
                    return Err("CICP assignment fields are inconsistent".into());
                }
            }
        }
        match self.kind {
            ChannelAssignmentKind::Speaker => {
                let azimuth = self
                    .azimuth_degrees
                    .ok_or("speaker assignment is missing azimuth")?;
                let elevation = self
                    .elevation_degrees
                    .ok_or("speaker assignment is missing elevation")?;
                validate_angles(azimuth, elevation)?;
                let compatible_role = match self.role {
                    ChannelRole::Positioned {
                        azimuth_degrees,
                        elevation_degrees,
                    } => azimuth_degrees == azimuth && elevation_degrees == elevation,
                    ChannelRole::Surround => annex_three_surround_weight(azimuth, elevation),
                    ChannelRole::Main | ChannelRole::DualMono => {
                        !annex_three_surround_weight(azimuth, elevation)
                    }
                    ChannelRole::Lfe => false,
                };
                if !compatible_role || self.component_index.is_some() {
                    return Err("speaker assignment fields are inconsistent".into());
                }
            }
            ChannelAssignmentKind::LowFrequencyEffects => {
                if self.role != ChannelRole::Lfe
                    || self.component_index.is_some()
                    || self.azimuth_degrees.is_some()
                    || self.elevation_degrees.is_some()
                {
                    return Err("LFE assignment fields are inconsistent".into());
                }
            }
            ChannelAssignmentKind::LegacyRole => {
                if self.cicp_position.is_some() || self.component_index.is_some() {
                    return Err("legacy-role assignment fields are inconsistent".into());
                }
                if let ChannelRole::Positioned {
                    azimuth_degrees,
                    elevation_degrees,
                } = self.role
                {
                    validate_angles(azimuth_degrees, elevation_degrees)?;
                    if self.azimuth_degrees != Some(azimuth_degrees)
                        || self.elevation_degrees != Some(elevation_degrees)
                    {
                        return Err("legacy positioned role fields are inconsistent".into());
                    }
                } else if self.azimuth_degrees.is_some() || self.elevation_degrees.is_some() {
                    return Err("legacy non-positioned role carries coordinates".into());
                }
            }
            ChannelAssignmentKind::Unassigned
            | ChannelAssignmentKind::Ambisonic
            | ChannelAssignmentKind::Object => {
                if self.role != ChannelRole::Main
                    || self.azimuth_degrees.is_some()
                    || self.elevation_degrees.is_some()
                    || (self.kind != ChannelAssignmentKind::Unassigned
                        && self.component_index.is_none())
                {
                    return Err("non-speaker assignment fields are inconsistent".into());
                }
            }
        }
        Ok(())
    }
}

/// Origin of the exact layout declaration.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChannelLayoutOrigin {
    CompatibilityDefault,
    Wave,
    Flac,
    IsoBmff,
    Decoder,
    ExplicitOverride,
    Renderer,
}

/// Parsed ISO-BMFF `chnl` fields and related MPEG-D `dmix` evidence.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IsoBmffChannelLayoutEvidence {
    version: u8,
    stream_structure: u8,
    format_ordering: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_channel_count: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    defined_layout: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel_order_definition: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    omitted_channels_map: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    object_count: Option<u8>,
    raw_chnl_sha256: String,
    dmix_sha256: Vec<String>,
}

impl IsoBmffChannelLayoutEvidence {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        version: u8,
        stream_structure: u8,
        format_ordering: u8,
        base_channel_count: Option<u8>,
        defined_layout: Option<u8>,
        channel_order_definition: Option<u8>,
        omitted_channels_map: Option<u64>,
        object_count: Option<u8>,
        raw_chnl_sha256: String,
        dmix_sha256: Vec<String>,
    ) -> Self {
        Self {
            version,
            stream_structure,
            format_ordering,
            base_channel_count,
            defined_layout,
            channel_order_definition,
            omitted_channels_map,
            object_count,
            raw_chnl_sha256,
            dmix_sha256,
        }
    }

    pub const fn version(&self) -> u8 {
        self.version
    }

    pub const fn stream_structure(&self) -> u8 {
        self.stream_structure
    }

    pub const fn format_ordering(&self) -> u8 {
        self.format_ordering
    }

    pub const fn base_channel_count(&self) -> Option<u8> {
        self.base_channel_count
    }

    pub const fn defined_layout(&self) -> Option<u8> {
        self.defined_layout
    }

    pub const fn channel_order_definition(&self) -> Option<u8> {
        self.channel_order_definition
    }

    pub const fn omitted_channels_map(&self) -> Option<u64> {
        self.omitted_channels_map
    }

    pub const fn object_count(&self) -> Option<u8> {
        self.object_count
    }

    pub fn raw_chnl_sha256(&self) -> &str {
        &self.raw_chnl_sha256
    }

    pub fn dmix_sha256(&self) -> &[String] {
        &self.dmix_sha256
    }

    fn validate(&self) -> Result<(), String> {
        let version_fields_are_consistent = match self.version {
            0 => self.base_channel_count.is_none() && self.format_ordering == 0,
            1 => self.base_channel_count.is_some(),
            _ => false,
        };
        let channel_fields_are_consistent = if self.stream_structure & 1 == 0 {
            self.defined_layout.is_none()
                && self.channel_order_definition.is_none()
                && self.omitted_channels_map.is_none()
        } else if self.defined_layout == Some(0) {
            self.channel_order_definition.is_none() && self.omitted_channels_map.is_none()
        } else {
            self.defined_layout.is_some()
                && self
                    .channel_order_definition
                    .is_some_and(|value| value <= 4)
                && (self.version != 0 || self.omitted_channels_map.is_some())
        };
        let object_fields_are_consistent =
            (self.stream_structure & 2 != 0) == self.object_count.is_some();
        if !version_fields_are_consistent
            || self.stream_structure == 0
            || self.stream_structure & !0x03 != 0
            || self.format_ordering > 2
            || !channel_fields_are_consistent
            || !object_fields_are_consistent
            || self.dmix_sha256.len() > 1024
            || !is_lower_hex_sha256(&self.raw_chnl_sha256)
            || self
                .dmix_sha256
                .iter()
                .any(|value| !is_lower_hex_sha256(value))
        {
            return Err("ISO-BMFF channel-layout evidence is invalid".into());
        }
        Ok(())
    }
}

/// Immutable identity of a renderer and its output configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RendererBinding {
    renderer: String,
    version: String,
    output_configuration: String,
    executable_sha256: String,
    settings_sha256: String,
}

impl RendererBinding {
    /// Construct a renderer binding from lower-case SHA-256 values.
    pub fn new(
        renderer: impl Into<String>,
        version: impl Into<String>,
        output_configuration: impl Into<String>,
        executable_sha256: impl Into<String>,
        settings_sha256: impl Into<String>,
    ) -> Result<Self, String> {
        let value = Self {
            renderer: renderer.into(),
            version: version.into(),
            output_configuration: output_configuration.into(),
            executable_sha256: executable_sha256.into(),
            settings_sha256: settings_sha256.into(),
        };
        value.validate()?;
        Ok(value)
    }

    pub fn renderer(&self) -> &str {
        &self.renderer
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn output_configuration(&self) -> &str {
        &self.output_configuration
    }

    pub fn executable_sha256(&self) -> &str {
        &self.executable_sha256
    }

    pub fn settings_sha256(&self) -> &str {
        &self.settings_sha256
    }

    fn validate(&self) -> Result<(), String> {
        if self.renderer.is_empty()
            || self.version.is_empty()
            || self.output_configuration.is_empty()
            || self.renderer.len() > 256
            || self.version.len() > 256
            || self.output_configuration.len() > 1024
            || !is_lower_hex_sha256(&self.executable_sha256)
            || !is_lower_hex_sha256(&self.settings_sha256)
        {
            return Err("renderer binding is incomplete or invalid".into());
        }
        Ok(())
    }
}

/// Exact layout and evidence carried beside stable PCM compatibility types.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelLayoutDescriptor {
    version: u32,
    assignments: Vec<ChannelAssignment>,
    provenance: ChannelLayoutProvenance,
    origin: ChannelLayoutOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    wave_channel_mask: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    flac_channel_mask: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    iso_bmff: Option<IsoBmffChannelLayoutEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    renderer: Option<RendererBinding>,
}

impl ChannelLayoutDescriptor {
    /// Construct a checked additive override from the established role model.
    pub fn from_channel_roles(roles: Vec<ChannelRole>) -> Result<Self, String> {
        if roles.is_empty() || roles.len() > usize::from(u16::MAX) {
            return Err("channel layout must contain 1..=65535 assignments".into());
        }
        for role in &roles {
            if let ChannelRole::Positioned {
                azimuth_degrees,
                elevation_degrees,
            } = *role
            {
                validate_angles(azimuth_degrees, elevation_degrees)?;
            }
        }
        Ok(Self {
            version: CHANNEL_LAYOUT_DESCRIPTOR_VERSION,
            assignments: roles
                .into_iter()
                .map(ChannelAssignment::legacy_role)
                .collect(),
            provenance: ChannelLayoutProvenance::KnownSpeakers,
            origin: ChannelLayoutOrigin::ExplicitOverride,
            wave_channel_mask: None,
            flac_channel_mask: None,
            iso_bmff: None,
            renderer: None,
        })
    }

    /// Construct a checked physical-speaker override.
    pub fn from_speakers(assignments: Vec<ChannelAssignment>) -> Result<Self, String> {
        let value = Self {
            version: CHANNEL_LAYOUT_DESCRIPTOR_VERSION,
            assignments,
            provenance: ChannelLayoutProvenance::KnownSpeakers,
            origin: ChannelLayoutOrigin::ExplicitOverride,
            wave_channel_mask: None,
            flac_channel_mask: None,
            iso_bmff: None,
            renderer: None,
        };
        value.validate()?;
        if value.assignments.iter().any(|assignment| {
            !matches!(
                assignment.kind,
                ChannelAssignmentKind::Speaker | ChannelAssignmentKind::LowFrequencyEffects
            )
        }) {
            return Err("physical-speaker override contains a non-speaker channel".into());
        }
        Ok(value)
    }

    /// Parse and validate the canonical JSON representation used by foreign APIs.
    pub fn from_json(json: &str) -> Result<Self, String> {
        if json.len() > MAX_CHANNEL_LAYOUT_JSON_BYTES {
            return Err(format!(
                "channel-layout JSON exceeds {MAX_CHANNEL_LAYOUT_JSON_BYTES} bytes"
            ));
        }
        let value: Self = serde_json::from_str(json)
            .map_err(|error| format!("invalid channel-layout JSON: {error}"))?;
        value.validate()?;
        Ok(value)
    }

    /// Serialize the versioned descriptor for C, Python, Wasm, REST, or gRPC.
    pub fn to_json(&self) -> Result<String, String> {
        self.validate()?;
        let json = serde_json::to_string(self)
            .map_err(|error| format!("serialize channel-layout descriptor: {error}"))?;
        if json.len() > MAX_CHANNEL_LAYOUT_JSON_BYTES {
            return Err(format!(
                "channel-layout JSON exceeds {MAX_CHANNEL_LAYOUT_JSON_BYTES} bytes"
            ));
        }
        Ok(json)
    }

    pub const fn version(&self) -> u32 {
        self.version
    }

    pub fn assignments(&self) -> &[ChannelAssignment] {
        &self.assignments
    }

    pub fn channel_count(&self) -> usize {
        self.assignments.len()
    }

    pub const fn provenance(&self) -> ChannelLayoutProvenance {
        self.provenance
    }

    pub const fn origin(&self) -> ChannelLayoutOrigin {
        self.origin
    }

    pub const fn wave_channel_mask(&self) -> Option<u32> {
        self.wave_channel_mask
    }

    pub const fn flac_channel_mask(&self) -> Option<u32> {
        self.flac_channel_mask
    }

    pub fn iso_bmff_evidence(&self) -> Option<&IsoBmffChannelLayoutEvidence> {
        self.iso_bmff.as_ref()
    }

    pub fn renderer_binding(&self) -> Option<&RendererBinding> {
        self.renderer.as_ref()
    }

    /// Compatibility roles in exact PCM-plane order.
    pub fn channel_roles(&self) -> Vec<ChannelRole> {
        self.assignments
            .iter()
            .map(ChannelAssignment::channel_role)
            .collect()
    }

    /// Whether every PCM plane is a physical speaker or LFE channel.
    pub fn is_measurement_ready(&self) -> bool {
        self.provenance == ChannelLayoutProvenance::KnownSpeakers
            && self.assignments.iter().all(|assignment| {
                matches!(
                    assignment.kind,
                    ChannelAssignmentKind::Speaker
                        | ChannelAssignmentKind::LowFrequencyEffects
                        | ChannelAssignmentKind::LegacyRole
                )
            })
    }

    /// Retain this descriptor's exact assignments when `roles` is either the
    /// same compatibility view or the generic view that WAVE serialization
    /// deterministically expands to it.
    pub(crate) fn assignments_compatible_with_roles(
        &self,
        roles: &[ChannelRole],
    ) -> Option<Vec<ChannelAssignment>> {
        if !self.is_measurement_ready() {
            return None;
        }
        let exact_roles = self.channel_roles();
        let compatible = exact_roles == roles
            || crate::wav::writer::persisted_channel_roles(roles)
                .is_ok_and(|persisted| persisted == exact_roles);
        compatible.then(|| self.assignments.clone())
    }

    /// Validate this descriptor for use as a caller-supplied speaker override.
    pub fn validate_override_for_channels(&self, channels: u16) -> Result<(), String> {
        self.validate()?;
        if self.origin != ChannelLayoutOrigin::ExplicitOverride
            || self.wave_channel_mask.is_some()
            || self.flac_channel_mask.is_some()
            || self.iso_bmff.is_some()
            || self.renderer.is_some()
        {
            return Err(
                "channel-layout override must have explicit-override origin and no source evidence"
                    .into(),
            );
        }
        if self.channel_count() != usize::from(channels) {
            return Err(format!(
                "channel layout has {} channels but input has {channels}",
                self.channel_count()
            ));
        }
        if !self.is_measurement_ready() {
            return Err("channel-layout override must identify every physical speaker".into());
        }
        Ok(())
    }

    pub(crate) fn wave(channels: u16, extensible: bool, mask: Option<u32>) -> Self {
        let (assignments, provenance) = assignments_from_wave_mask(channels, mask, extensible);
        Self {
            version: CHANNEL_LAYOUT_DESCRIPTOR_VERSION,
            assignments,
            provenance,
            origin: ChannelLayoutOrigin::Wave,
            wave_channel_mask: mask,
            flac_channel_mask: None,
            iso_bmff: None,
            renderer: None,
        }
    }

    pub(crate) fn flac(channels: u16, mask: Option<u32>) -> Self {
        let effective_mask = mask.or_else(|| default_flac_channel_mask(channels));
        let (assignments, provenance) = assignments_from_wave_mask(channels, effective_mask, true);
        Self {
            version: CHANNEL_LAYOUT_DESCRIPTOR_VERSION,
            assignments,
            provenance,
            origin: ChannelLayoutOrigin::Flac,
            wave_channel_mask: None,
            flac_channel_mask: mask,
            iso_bmff: None,
            renderer: None,
        }
    }

    pub(crate) fn decoded(
        assignments: Vec<ChannelAssignment>,
        provenance: ChannelLayoutProvenance,
    ) -> Self {
        Self {
            version: CHANNEL_LAYOUT_DESCRIPTOR_VERSION,
            assignments,
            provenance,
            origin: ChannelLayoutOrigin::Decoder,
            wave_channel_mask: None,
            flac_channel_mask: None,
            iso_bmff: None,
            renderer: None,
        }
    }

    pub(crate) fn decoded_from_roles(
        roles: &[ChannelRole],
        provenance: ChannelLayoutProvenance,
    ) -> Self {
        let assignments = match provenance {
            ChannelLayoutProvenance::KnownSpeakers => roles
                .iter()
                .copied()
                .map(ChannelAssignment::legacy_role)
                .collect(),
            ChannelLayoutProvenance::Unknown => (0..roles.len())
                .map(|index| ChannelAssignment::unassigned(index as u32))
                .collect(),
            ChannelLayoutProvenance::SceneBased => (0..roles.len())
                .map(|index| ChannelAssignment::ambisonic(index as u32))
                .collect(),
        };
        Self::decoded(assignments, provenance)
    }

    pub(crate) fn iso_bmff(
        assignments: Vec<ChannelAssignment>,
        provenance: ChannelLayoutProvenance,
        evidence: IsoBmffChannelLayoutEvidence,
    ) -> Self {
        Self {
            version: CHANNEL_LAYOUT_DESCRIPTOR_VERSION,
            assignments,
            provenance,
            origin: ChannelLayoutOrigin::IsoBmff,
            wave_channel_mask: None,
            flac_channel_mask: None,
            iso_bmff: Some(evidence),
            renderer: None,
        }
    }

    /// Bind a rendered speaker layout to the exact renderer and settings.
    pub fn rendered(
        assignments: Vec<ChannelAssignment>,
        renderer: RendererBinding,
    ) -> Result<Self, String> {
        let value = Self {
            version: CHANNEL_LAYOUT_DESCRIPTOR_VERSION,
            assignments,
            provenance: ChannelLayoutProvenance::KnownSpeakers,
            origin: ChannelLayoutOrigin::Renderer,
            wave_channel_mask: None,
            flac_channel_mask: None,
            iso_bmff: None,
            renderer: Some(renderer),
        };
        value.validate()?;
        if !value.is_measurement_ready() {
            return Err("renderer output layout is not a complete speaker layout".into());
        }
        Ok(value)
    }

    pub(crate) fn with_origin(mut self, origin: ChannelLayoutOrigin) -> Self {
        self.origin = origin;
        self
    }

    pub(crate) fn with_provenance(mut self, provenance: ChannelLayoutProvenance) -> Self {
        self.provenance = provenance;
        self
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.version != CHANNEL_LAYOUT_DESCRIPTOR_VERSION {
            return Err(format!(
                "unsupported channel-layout descriptor version {}",
                self.version
            ));
        }
        if self.assignments.is_empty() || self.assignments.len() > usize::from(u16::MAX) {
            return Err("channel layout must contain 1..=65535 assignments".into());
        }
        for assignment in &self.assignments {
            assignment.validate()?;
        }
        if let Some(evidence) = &self.iso_bmff {
            evidence.validate()?;
        }
        if let Some(renderer) = &self.renderer {
            renderer.validate()?;
        }
        if self.origin == ChannelLayoutOrigin::IsoBmff && self.iso_bmff.is_none()
            || self.origin != ChannelLayoutOrigin::IsoBmff && self.iso_bmff.is_some()
            || self.origin == ChannelLayoutOrigin::Renderer && self.renderer.is_none()
            || self.origin != ChannelLayoutOrigin::Renderer && self.renderer.is_some()
            || self.origin != ChannelLayoutOrigin::Wave && self.wave_channel_mask.is_some()
            || self.origin != ChannelLayoutOrigin::Flac && self.flac_channel_mask.is_some()
        {
            return Err("channel-layout origin and evidence are inconsistent".into());
        }
        let physical = self.assignments.iter().all(|assignment| {
            matches!(
                assignment.kind,
                ChannelAssignmentKind::Speaker
                    | ChannelAssignmentKind::LowFrequencyEffects
                    | ChannelAssignmentKind::LegacyRole
            )
        });
        if self.provenance == ChannelLayoutProvenance::KnownSpeakers && !physical {
            return Err("known-speaker provenance contains a non-speaker assignment".into());
        }
        if self.provenance == ChannelLayoutProvenance::SceneBased
            && !self.assignments.iter().any(|assignment| {
                matches!(
                    assignment.kind,
                    ChannelAssignmentKind::Ambisonic | ChannelAssignmentKind::Object
                )
            })
        {
            return Err("scene-based layout contains no scene channels".into());
        }
        Ok(())
    }
}

fn validate_angles(azimuth_degrees: i16, elevation_degrees: i16) -> Result<(), String> {
    if !(-180..=180).contains(&azimuth_degrees) {
        return Err("speaker azimuth must be in -180..=180 degrees".into());
    }
    if !(-90..=90).contains(&elevation_degrees) {
        return Err("speaker elevation must be in -90..=90 degrees".into());
    }
    Ok(())
}

fn validate_cicp_position(position: u8) -> Result<(), String> {
    if matches!(position, 0..=31 | 36..=42 | 126) {
        Ok(())
    } else if position == 127 {
        Err("CICP position 127 is unknown; encode it as an unassigned plane".into())
    } else {
        Err(format!("CICP position {position} is reserved"))
    }
}

const fn annex_three_surround_weight(azimuth_degrees: i16, elevation_degrees: i16) -> bool {
    let azimuth = azimuth_degrees.unsigned_abs();
    elevation_degrees.unsigned_abs() < 30 && azimuth >= 60 && azimuth <= 120
}

fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// RFC 9639 section 9.1.3 default channel order expressed as WAVE speaker
/// bits. More than eight channels have no implicit FLAC assignment.
pub(crate) const fn default_flac_channel_mask(channels: u16) -> Option<u32> {
    Some(match channels {
        1 => 0x0004,
        2 => 0x0003,
        3 => 0x0007,
        4 => 0x0033,
        5 => 0x0037,
        6 => 0x003f,
        7 => 0x070f,
        8 => 0x063f,
        _ => return None,
    })
}

fn assignments_from_wave_mask(
    channels: u16,
    mask: Option<u32>,
    positional: bool,
) -> (Vec<ChannelAssignment>, ChannelLayoutProvenance) {
    let known_legacy = !positional && matches!(channels, 1 | 2);
    let Some(mask) = mask else {
        return (
            if known_legacy {
                crate::wav::default_channel_roles(channels)
                    .into_iter()
                    .map(ChannelAssignment::legacy_role)
                    .collect()
            } else {
                (0..u32::from(channels))
                    .map(ChannelAssignment::unassigned)
                    .collect()
            },
            if known_legacy {
                ChannelLayoutProvenance::KnownSpeakers
            } else {
                ChannelLayoutProvenance::Unknown
            },
        );
    };

    let mut assignments = Vec::with_capacity(usize::from(channels));
    for bit in 0..32_u8 {
        if mask & (1_u32 << bit) != 0 && assignments.len() < usize::from(channels) {
            assignments.push(if bit <= 17 {
                ChannelAssignment::cicp(wave_bit_to_cicp_for_mask(bit, mask))
            } else {
                ChannelAssignment::unassigned(u32::from(bit))
            });
        }
    }
    while assignments.len() < usize::from(channels) {
        assignments.push(ChannelAssignment::unassigned(assignments.len() as u32));
    }
    let standard =
        mask != 0 && mask & !((1_u32 << 18) - 1) == 0 && mask.count_ones() == u32::from(channels);
    if standard {
        assignments = assignments
            .into_iter()
            .zip(crate::wav::reader::roles_from_wave_mask(mask, channels))
            .map(|(assignment, role)| assignment.with_compatibility_role(role))
            .collect();
    }
    (
        assignments,
        if standard {
            ChannelLayoutProvenance::KnownSpeakers
        } else {
            ChannelLayoutProvenance::Unknown
        },
    )
}

pub(crate) const fn wave_bit_to_cicp(bit: u8) -> u8 {
    match bit {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 8,
        5 => 9,
        6 => 6,
        7 => 7,
        8 => 10,
        9 => 13,
        10 => 14,
        11 => 25,
        12 => 17,
        13 => 19,
        14 => 18,
        15 => 20,
        16 => 22,
        17 => 21,
        _ => 127,
    }
}

pub(crate) const fn wave_mask_uses_conventional_five_x_surround(mask: u32) -> bool {
    mask & 0x0000_0037 == 0x0000_0037 && mask & (0x0000_00c0 | 0x0000_0700) == 0
}

pub(crate) const fn wave_bit_to_cicp_for_mask(bit: u8, mask: u32) -> u8 {
    match bit {
        4 if wave_mask_uses_conventional_five_x_surround(mask) => 4,
        5 if wave_mask_uses_conventional_five_x_surround(mask) => 5,
        _ => wave_bit_to_cicp(bit),
    }
}

/// Nominal CICP geometry projected into Forge's left-negative convention.
fn cicp_role(position: u8) -> (ChannelRole, Option<i16>, Option<i16>) {
    let coordinates = match position {
        0 => Some((-30, 0)),
        1 => Some((30, 0)),
        2 => Some((0, 0)),
        3 | 26 | 36 => return (ChannelRole::Lfe, None, None),
        4 | 11 => Some((-110, 0)),
        5 | 12 => Some((110, 0)),
        6 => Some((-15, 0)),
        7 => Some((15, 0)),
        8 => Some((-135, 0)),
        9 => Some((135, 0)),
        10 => Some((180, 0)),
        13 => Some((-90, 0)),
        14 => Some((90, 0)),
        15 => Some((-60, 0)),
        16 => Some((60, 0)),
        17 => Some((-30, 45)),
        18 => Some((30, 45)),
        19 => Some((0, 45)),
        20 => Some((-135, 45)),
        21 => Some((135, 45)),
        22 => Some((180, 45)),
        23 => Some((-90, 45)),
        24 => Some((90, 45)),
        25 => Some((0, 90)),
        27 => Some((-30, -30)),
        28 => Some((30, -30)),
        29 => Some((0, -30)),
        30 => Some((-110, 45)),
        31 => Some((110, 45)),
        37 => Some((-45, 0)),
        38 => Some((45, 0)),
        39 => Some((-22, 0)),
        40 => Some((22, 0)),
        41 => Some((-150, 0)),
        42 => Some((150, 0)),
        _ => None,
    };
    coordinates.map_or((ChannelRole::Main, None, None), |(azimuth, elevation)| {
        (
            ChannelRole::positioned(azimuth, elevation),
            Some(azimuth),
            Some(elevation),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_wave_mask_keeps_assigned_and_unassigned_planes() {
        let layout = ChannelLayoutDescriptor::wave(4, true, Some(0x3));
        assert_eq!(layout.provenance(), ChannelLayoutProvenance::Unknown);
        assert_eq!(layout.assignments()[0].cicp_position(), Some(0));
        assert_eq!(layout.assignments()[1].cicp_position(), Some(1));
        assert_eq!(
            layout.assignments()[2].kind(),
            ChannelAssignmentKind::Unassigned
        );
        assert_eq!(
            layout.assignments()[3].kind(),
            ChannelAssignmentKind::Unassigned
        );
    }

    #[test]
    fn non_default_wave_mask_retains_exact_standard_identity() {
        let layout = ChannelLayoutDescriptor::wave(4, true, Some(0x5003));
        assert_eq!(layout.provenance(), ChannelLayoutProvenance::KnownSpeakers);
        assert_eq!(layout.wave_channel_mask(), Some(0x5003));
        let positions = layout
            .assignments()
            .iter()
            .map(ChannelAssignment::cicp_position)
            .collect::<Vec<_>>();
        assert_eq!(positions, [Some(0), Some(1), Some(17), Some(18)]);
    }

    #[test]
    fn descriptor_json_is_checked_and_round_trips() {
        let layout = ChannelLayoutDescriptor::from_speakers(vec![
            ChannelAssignment::speaker(-90, 0).unwrap(),
            ChannelAssignment::low_frequency_effects(),
        ])
        .unwrap();
        let json = layout.to_json().unwrap();
        assert_eq!(ChannelLayoutDescriptor::from_json(&json).unwrap(), layout);

        let inconsistent = r#"{"version":1,"assignments":[{"kind":"speaker","role":"main","azimuth_degrees":-90,"elevation_degrees":0}],"provenance":"known-speakers","origin":"explicit-override"}"#;
        assert!(ChannelLayoutDescriptor::from_json(inconsistent).is_err());
        let false_lfe = r#"{"version":1,"assignments":[{"kind":"low-frequency-effects","role":"lfe","cicp_position":0}],"provenance":"known-speakers","origin":"explicit-override"}"#;
        assert!(ChannelLayoutDescriptor::from_json(false_lfe).is_err());
    }

    #[test]
    fn descriptor_json_matches_the_published_schema() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../schema/channel-layout-v1.schema.json")).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let descriptors = [
            ChannelLayoutDescriptor::wave(6, true, Some(0x003f)),
            ChannelLayoutDescriptor::wave(1, true, Some(1 << 18)),
            ChannelLayoutDescriptor::from_channel_roles(vec![ChannelRole::Main]).unwrap(),
            ChannelLayoutDescriptor::rendered(
                vec![ChannelAssignment::speaker(0, 0).unwrap()],
                RendererBinding::new("renderer", "1.0", "mono", "0".repeat(64), "a".repeat(64))
                    .unwrap(),
            )
            .unwrap(),
        ];
        for descriptor in descriptors {
            let value = serde_json::to_value(descriptor).unwrap();
            assert!(validator.is_valid(&value), "{value:#}");
        }
    }

    #[test]
    fn cicp_annex_three_boundaries_project_exactly() {
        assert_eq!(
            ChannelAssignment::cicp(15).channel_role(),
            ChannelRole::positioned(-60, 0)
        );
        assert_eq!(
            ChannelAssignment::cicp(16).channel_role(),
            ChannelRole::positioned(60, 0)
        );
        assert_eq!(
            ChannelAssignment::cicp(23).channel_role(),
            ChannelRole::positioned(-90, 45)
        );
        assert_eq!(ChannelAssignment::cicp(26).channel_role(), ChannelRole::Lfe);
    }

    #[test]
    fn renderer_binding_requires_reproducible_hashes() {
        assert!(
            RendererBinding::new("renderer", "1", "stereo", "0".repeat(64), "a".repeat(64)).is_ok()
        );
        assert!(RendererBinding::new("renderer", "1", "stereo", "ABC", "a".repeat(64)).is_err());
    }

    #[test]
    fn renderer_compatibility_retains_exact_wave_speaker_ids() {
        let decoded = ChannelLayoutDescriptor::wave(6, true, Some(0x003f));
        decoded.validate().unwrap();
        decoded.to_json().unwrap();
        let generic = crate::wav::named_channel_layout("5.1").unwrap();
        let assignments = decoded.assignments_compatible_with_roles(&generic).unwrap();
        assert_eq!(
            assignments
                .iter()
                .map(ChannelAssignment::cicp_position)
                .collect::<Vec<_>>(),
            [Some(0), Some(1), Some(2), Some(3), Some(4), Some(5)]
        );
    }
}
