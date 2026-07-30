//! Named loudness targets for common playback and delivery contexts.
//!
//! Platform behaviour can change independently of Forge. Platform presets
//! therefore carry a versioned identifier, evidence classification, source,
//! and verification date. The short names remain reproducible aliases to one
//! explicitly identified profile; they are not presented as timeless service
//! contracts.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileEvidence {
    /// The platform publishes the numeric target and ceiling/headroom guidance.
    PublishedPlatformPolicy,
    /// The platform documents the feature, but not Forge's numeric reference.
    EngineeringReference,
}

impl ProfileEvidence {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PublishedPlatformPolicy => "published-platform-policy",
            Self::EngineeringReference => "engineering-reference",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileProvenance {
    /// Stable identifier for the exact built-in profile revision.
    pub profile_id: &'static str,
    /// First-party page used to classify the profile's evidence.
    pub source_url: &'static str,
    /// Publication date shown by the source, when the source provides one.
    pub source_date: Option<&'static str>,
    /// Date on which Forge maintainers rechecked the first-party page.
    pub checked_on: &'static str,
    pub evidence: ProfileEvidence,
    /// Important scope or evidence limitation shown to the user.
    pub caveat: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Preset {
    pub name: &'static str,
    pub target_lufs: f64,
    pub ceiling_db: f64,
    pub description: &'static str,
    pub provenance: Option<ProfileProvenance>,
}

impl Preset {
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "spotify" | "spotify-normal-2026-07-30" => Some(Self {
                name: "spotify-normal-2026-07-30",
                target_lufs: -14.0,
                ceiling_db: -1.0,
                description: "Spotify Normal playback/mastering guidance",
                provenance: Some(ProfileProvenance {
                    profile_id: "spotify-normal-2026-07-30",
                    source_url:
                        "https://support.spotify.com/artists/article/loudness-normalization/",
                    source_date: None,
                    checked_on: "2026-07-30",
                    evidence: ProfileEvidence::PublishedPlatformPolicy,
                    caveat: "playback normalization varies by listener setting, client, and device",
                }),
            }),
            "apple-music" | "apple-music-reference-2026-07-30" => Some(Self {
                name: "apple-music-reference-2026-07-30",
                target_lufs: -16.0,
                ceiling_db: -1.0,
                description: "Forge Apple Music Sound Check engineering reference",
                provenance: Some(ProfileProvenance {
                    profile_id: "apple-music-reference-2026-07-30",
                    source_url: "https://support.apple.com/en-us/109331",
                    source_date: Some("2025-03-26"),
                    checked_on: "2026-07-30",
                    evidence: ProfileEvidence::EngineeringReference,
                    caveat:
                        "Apple documents Sound Check behaviour but does not publish these numeric values",
                }),
            }),
            "youtube" | "youtube-reference-2026-07-30" => Some(Self {
                name: "youtube-reference-2026-07-30",
                target_lufs: -14.0,
                ceiling_db: -1.0,
                description: "Forge YouTube playback engineering reference",
                provenance: Some(ProfileProvenance {
                    profile_id: "youtube-reference-2026-07-30",
                    source_url: "https://support.google.com/youtube/answer/16619284",
                    source_date: None,
                    checked_on: "2026-07-30",
                    evidence: ProfileEvidence::EngineeringReference,
                    caveat:
                        "YouTube documents variable audio enhancement but not these numeric values",
                }),
            }),
            "podcast-stereo" => Some(Self {
                name: "podcast-stereo",
                target_lufs: -16.0,
                ceiling_db: -1.0,
                description: "common stereo podcast delivery reference",
                provenance: None,
            }),
            "podcast-mono" => Some(Self {
                name: "podcast-mono",
                target_lufs: -19.0,
                ceiling_db: -1.0,
                description: "common mono podcast delivery reference",
                provenance: None,
            }),
            "ebu-r128" => Some(Self {
                name: "ebu-r128",
                target_lufs: -23.0,
                ceiling_db: -1.0,
                description: "EBU R 128 programme loudness",
                provenance: None,
            }),
            "atsc-a85" => Some(Self {
                name: "atsc-a85",
                target_lufs: -24.0,
                ceiling_db: -2.0,
                description: "ATSC A/85 television delivery",
                provenance: None,
            }),
            "arib-tr-b32" => Some(Self {
                name: "arib-tr-b32",
                target_lufs: -24.0,
                ceiling_db: -1.0,
                description: "ARIB TR-B32 Japanese digital television delivery",
                provenance: None,
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_presets_match_published_targets() {
        let ebu = Preset::named("ebu-r128").unwrap();
        assert_eq!((ebu.target_lufs, ebu.ceiling_db), (-23.0, -1.0));
        let atsc = Preset::named("atsc-a85").unwrap();
        assert_eq!((atsc.target_lufs, atsc.ceiling_db), (-24.0, -2.0));
        let arib = Preset::named("arib-tr-b32").unwrap();
        assert_eq!((arib.target_lufs, arib.ceiling_db), (-24.0, -1.0));
    }

    #[test]
    fn unknown_preset_is_rejected() {
        assert!(Preset::named("unknown").is_none());
    }

    #[test]
    fn platform_aliases_resolve_to_versioned_profiles() {
        for (alias, profile_id) in [
            ("spotify", "spotify-normal-2026-07-30"),
            ("apple-music", "apple-music-reference-2026-07-30"),
            ("youtube", "youtube-reference-2026-07-30"),
        ] {
            let alias_profile = Preset::named(alias).unwrap();
            let versioned_profile = Preset::named(profile_id).unwrap();
            assert_eq!(alias_profile, versioned_profile);
            assert_eq!(alias_profile.name, profile_id);
            assert_eq!(alias_profile.provenance.unwrap().profile_id, profile_id);
        }
    }

    #[test]
    fn platform_profiles_expose_source_date_and_evidence() {
        let spotify = Preset::named("spotify").unwrap().provenance.unwrap();
        assert_eq!(spotify.evidence, ProfileEvidence::PublishedPlatformPolicy);

        for alias in ["spotify", "apple-music", "youtube"] {
            let source = Preset::named(alias).unwrap().provenance.unwrap();
            assert!(source.source_url.starts_with("https://"));
            assert_eq!(source.checked_on, "2026-07-30");
            assert!(!source.caveat.is_empty());
        }

        for alias in ["apple-music", "youtube"] {
            assert_eq!(
                Preset::named(alias).unwrap().provenance.unwrap().evidence,
                ProfileEvidence::EngineeringReference
            );
        }
    }
}
