//! Explicit WAVE-order downmix matrices used by immersive presentation QC.
//!
//! The matrices in this module are deliberately small and auditable. They are
//! not a renderer for an object-based codec: callers must provide the decoded
//! channel layout, and every output coefficient is returned in the report.

use crate::wav::{named_channel_layout, AudioBuffer, ChannelRole, PcmKind};
use serde::{Deserialize, Serialize};

const HALF_POWER: f32 = std::f32::consts::FRAC_1_SQRT_2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Mono,
    Stereo,
    FiveOne,
    SixOne,
    SevenOne,
    FiveOneFour,
    SevenOneFour,
}

impl Layout {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "mono" | "1.0" => Some(Self::Mono),
            "stereo" | "2.0" => Some(Self::Stereo),
            "5.1" => Some(Self::FiveOne),
            "6.1" => Some(Self::SixOne),
            "7.1" => Some(Self::SevenOne),
            "5.1.4" => Some(Self::FiveOneFour),
            "7.1.4" => Some(Self::SevenOneFour),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mono => "mono",
            Self::Stereo => "stereo",
            Self::FiveOne => "5.1",
            Self::SixOne => "6.1",
            Self::SevenOne => "7.1",
            Self::FiveOneFour => "5.1.4",
            Self::SevenOneFour => "7.1.4",
        }
    }

    pub const fn channels(self) -> usize {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::FiveOne => 6,
            Self::SixOne => 7,
            Self::SevenOne => 8,
            Self::FiveOneFour => 10,
            Self::SevenOneFour => 12,
        }
    }

    pub fn labels(self) -> &'static [&'static str] {
        match self {
            Self::Mono => &["M"],
            Self::Stereo => &["FL", "FR"],
            Self::FiveOne => &["FL", "FR", "FC", "LFE", "BL", "BR"],
            Self::SixOne => &["FL", "FR", "FC", "LFE", "BC", "SL", "SR"],
            Self::SevenOne => &["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"],
            Self::FiveOneFour => &[
                "FL", "FR", "FC", "LFE", "BL", "BR", "TFL", "TFR", "TBL", "TBR",
            ],
            Self::SevenOneFour => &[
                "FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR", "TFL", "TFR", "TBL", "TBR",
            ],
        }
    }

    pub fn roles(self) -> Vec<ChannelRole> {
        named_channel_layout(self.as_str()).unwrap_or_else(|| match self {
            Self::Mono | Self::Stereo => vec![ChannelRole::Main; self.channels()],
            _ => Vec::new(),
        })
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub enum Profile {
    #[serde(rename = "stereo")]
    Stereo,
    #[serde(rename = "5.1")]
    FiveOne,
    #[serde(rename = "7.1.4")]
    SevenOneFour,
}

impl Profile {
    pub const fn target_layout(self) -> Layout {
        match self {
            Self::Stereo => Layout::Stereo,
            Self::FiveOne => Layout::FiveOne,
            Self::SevenOneFour => Layout::SevenOneFour,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stereo => "stereo",
            Self::FiveOne => "5.1",
            Self::SevenOneFour => "7.1.4",
        }
    }

    pub const fn method(self) -> &'static str {
        match self {
            Self::Stereo => {
                "stereo-loro-v1: FL/FR unity, FC/non-LFE surround/height at -3.01 dB, LFE omitted"
            }
            Self::FiveOne => {
                "5.1-film-v1: base channels unity, expanded side/height channels at -3.01 dB"
            }
            Self::SevenOneFour => "7.1.4-identity-v1: WAVE-order identity verification",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MatrixTerm {
    pub input_channel: usize,
    pub input_label: &'static str,
    pub coefficient: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct MatrixChannel {
    pub output_channel: usize,
    pub output_label: &'static str,
    pub terms: Vec<MatrixTerm>,
}

#[derive(Debug, Clone)]
pub struct RenderedDownmix {
    pub buffer: AudioBuffer,
    pub mapping: Vec<MatrixChannel>,
    pub method: &'static str,
}

/// Apply an explicit immersive downmix profile to a decoded WAVE-order buffer.
pub fn render(
    source: &AudioBuffer,
    input_layout: Layout,
    profile: Profile,
) -> Result<RenderedDownmix, String> {
    if source.channels as usize != input_layout.channels()
        || source.data.len() != input_layout.channels()
    {
        return Err(format!(
            "input layout {} requires {} channels, decoded {}",
            input_layout.as_str(),
            input_layout.channels(),
            source.channels
        ));
    }
    if source
        .data
        .iter()
        .any(|channel| channel.len() != source.frames)
    {
        return Err("decoded channel frame counts are inconsistent".into());
    }
    let mapping = matrix(input_layout, profile)?;
    let mut data = vec![vec![0.0f32; source.frames]; mapping.len()];
    for (destination, channel) in data.iter_mut().zip(&mapping) {
        for term in &channel.terms {
            let input = &source.data[term.input_channel - 1];
            for (output, sample) in destination.iter_mut().zip(input) {
                *output += *sample * term.coefficient;
            }
        }
    }
    let target = profile.target_layout();
    Ok(RenderedDownmix {
        buffer: AudioBuffer {
            sample_rate: source.sample_rate,
            channels: target.channels() as u16,
            frames: source.frames,
            data,
            channel_roles: target.roles(),
            source_kind: PcmKind::F32,
        },
        mapping,
        method: profile.method(),
    })
}

fn matrix(input: Layout, profile: Profile) -> Result<Vec<MatrixChannel>, String> {
    let target = profile.target_layout();
    if input == target {
        return Ok(identity(input));
    }
    match profile {
        Profile::Stereo => stereo_matrix(input),
        Profile::FiveOne => five_one_matrix(input),
        Profile::SevenOneFour => Err(format!(
            "profile 7.1.4 only accepts a 7.1.4 source; {} would be an upmix",
            input.as_str()
        )),
    }
}

fn identity(layout: Layout) -> Vec<MatrixChannel> {
    layout
        .labels()
        .iter()
        .enumerate()
        .map(|(index, label)| MatrixChannel {
            output_channel: index + 1,
            output_label: label,
            terms: vec![MatrixTerm {
                input_channel: index + 1,
                input_label: label,
                coefficient: 1.0,
            }],
        })
        .collect()
}

fn stereo_matrix(input: Layout) -> Result<Vec<MatrixChannel>, String> {
    let mut left = Vec::new();
    let mut right = Vec::new();
    if input == Layout::Mono {
        add(&mut left, input, "M", 1.0)?;
        add(&mut right, input, "M", 1.0)?;
    } else {
        add(&mut left, input, "FL", 1.0)?;
        add(&mut right, input, "FR", 1.0)?;
        if has(input, "FC") {
            add(&mut left, input, "FC", HALF_POWER)?;
            add(&mut right, input, "FC", HALF_POWER)?;
        }
        for label in ["BL", "SL", "BC", "TBL", "TFL"] {
            if has(input, label) {
                add(&mut left, input, label, HALF_POWER)?;
            }
        }
        for label in ["BR", "SR", "BC", "TBR", "TFR"] {
            if has(input, label) {
                add(&mut right, input, label, HALF_POWER)?;
            }
        }
    }
    Ok(vec![
        channel(Layout::Stereo, 1, "FL", left),
        channel(Layout::Stereo, 2, "FR", right),
    ])
}

fn five_one_matrix(input: Layout) -> Result<Vec<MatrixChannel>, String> {
    let channels = match input {
        Layout::FiveOneFour => vec![
            terms(input, &[("FL", 1.0), ("TFL", HALF_POWER)])?,
            terms(input, &[("FR", 1.0), ("TFR", HALF_POWER)])?,
            terms(input, &[("FC", 1.0)])?,
            terms(input, &[("LFE", 1.0)])?,
            terms(input, &[("BL", 1.0), ("TBL", HALF_POWER)])?,
            terms(input, &[("BR", 1.0), ("TBR", HALF_POWER)])?,
        ],
        Layout::SevenOneFour => vec![
            terms(input, &[("FL", 1.0), ("TFL", HALF_POWER)])?,
            terms(input, &[("FR", 1.0), ("TFR", HALF_POWER)])?,
            terms(input, &[("FC", 1.0)])?,
            terms(input, &[("LFE", 1.0)])?,
            terms(
                input,
                &[("BL", 1.0), ("SL", HALF_POWER), ("TBL", HALF_POWER)],
            )?,
            terms(
                input,
                &[("BR", 1.0), ("SR", HALF_POWER), ("TBR", HALF_POWER)],
            )?,
        ],
        Layout::SevenOne => vec![
            terms(input, &[("FL", 1.0)])?,
            terms(input, &[("FR", 1.0)])?,
            terms(input, &[("FC", 1.0)])?,
            terms(input, &[("LFE", 1.0)])?,
            terms(input, &[("BL", 1.0), ("SL", HALF_POWER)])?,
            terms(input, &[("BR", 1.0), ("SR", HALF_POWER)])?,
        ],
        Layout::SixOne => vec![
            terms(input, &[("FL", 1.0)])?,
            terms(input, &[("FR", 1.0)])?,
            terms(input, &[("FC", 1.0)])?,
            terms(input, &[("LFE", 1.0)])?,
            terms(input, &[("BC", HALF_POWER), ("SL", 1.0)])?,
            terms(input, &[("BC", HALF_POWER), ("SR", 1.0)])?,
        ],
        _ => {
            return Err(format!(
                "profile 5.1 does not downmix a {} source",
                input.as_str()
            ));
        }
    };
    Ok(channels
        .into_iter()
        .enumerate()
        .map(|(index, terms)| {
            channel(
                Layout::FiveOne,
                index + 1,
                Layout::FiveOne.labels()[index],
                terms,
            )
        })
        .collect())
}

fn channel(
    layout: Layout,
    output_channel: usize,
    output_label: &'static str,
    terms: Vec<MatrixTerm>,
) -> MatrixChannel {
    debug_assert_eq!(layout.labels()[output_channel - 1], output_label);
    MatrixChannel {
        output_channel,
        output_label,
        terms,
    }
}

fn terms(layout: Layout, values: &[(&'static str, f32)]) -> Result<Vec<MatrixTerm>, String> {
    values
        .iter()
        .map(|(label, coefficient)| {
            let index = layout
                .labels()
                .iter()
                .position(|candidate| candidate == label)
                .ok_or_else(|| format!("{} source has no {label} channel", layout.as_str()))?;
            Ok(MatrixTerm {
                input_channel: index + 1,
                input_label: label,
                coefficient: *coefficient,
            })
        })
        .collect()
}

fn add(
    destination: &mut Vec<MatrixTerm>,
    layout: Layout,
    label: &'static str,
    coefficient: f32,
) -> Result<(), String> {
    destination.extend(terms(layout, &[(label, coefficient)])?);
    Ok(())
}

fn has(layout: Layout, label: &str) -> bool {
    layout.labels().contains(&label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_named_layouts_and_keeps_wave_order() {
        assert_eq!(Layout::parse("7.1.4").unwrap().channels(), 12);
        assert_eq!(Layout::SevenOneFour.labels()[8], "TFL");
        assert_eq!(Layout::SevenOneFour.roles().len(), 12);
    }

    #[test]
    fn stereo_matrix_omits_lfe_and_reports_explicit_coefficients() {
        let source = AudioBuffer {
            sample_rate: 48_000,
            channels: 6,
            frames: 1,
            data: vec![vec![0.0]; 6],
            channel_roles: Layout::FiveOne.roles(),
            source_kind: PcmKind::F32,
        };
        let rendered = render(&source, Layout::FiveOne, Profile::Stereo).unwrap();
        assert_eq!(rendered.mapping[0].terms.len(), 3);
        assert!(rendered.mapping[0]
            .terms
            .iter()
            .all(|term| term.input_label != "LFE"));
        assert_eq!(rendered.mapping[0].terms[1].coefficient, HALF_POWER);
    }

    #[test]
    fn five_one_from_seven_one_four_routes_heights_and_sides() {
        let source = AudioBuffer {
            sample_rate: 48_000,
            channels: 12,
            frames: 1,
            data: vec![vec![0.0]; 12],
            channel_roles: Layout::SevenOneFour.roles(),
            source_kind: PcmKind::F32,
        };
        let rendered = render(&source, Layout::SevenOneFour, Profile::FiveOne).unwrap();
        let surrounds = &rendered.mapping[4].terms;
        assert_eq!(surrounds.len(), 3);
        assert_eq!(surrounds[1].input_label, "SL");
        assert_eq!(surrounds[2].input_label, "TBL");
    }

    #[test]
    fn seven_one_four_profile_is_identity_only() {
        let source = AudioBuffer {
            sample_rate: 48_000,
            channels: 12,
            frames: 1,
            data: vec![vec![0.0]; 12],
            channel_roles: Layout::SevenOneFour.roles(),
            source_kind: PcmKind::F32,
        };
        let rendered = render(&source, Layout::SevenOneFour, Profile::SevenOneFour).unwrap();
        assert!(rendered
            .mapping
            .iter()
            .all(|channel| channel.terms.len() == 1));
        assert!(render(&source, Layout::SevenOne, Profile::SevenOneFour).is_err());
    }
}
