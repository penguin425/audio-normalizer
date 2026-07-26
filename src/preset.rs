//! Named loudness targets for common playback and delivery contexts.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Preset {
    pub name: &'static str,
    pub target_lufs: f64,
    pub ceiling_db: f64,
    pub description: &'static str,
}

impl Preset {
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "spotify" => Some(Self {
                name: "spotify",
                target_lufs: -14.0,
                ceiling_db: -1.0,
                description: "Spotify Normal playback/mastering guidance",
            }),
            "apple-music" => Some(Self {
                name: "apple-music",
                target_lufs: -16.0,
                ceiling_db: -1.0,
                description: "Apple Music Sound Check playback reference",
            }),
            "youtube" => Some(Self {
                name: "youtube",
                target_lufs: -14.0,
                ceiling_db: -1.0,
                description: "YouTube playback-normalization reference",
            }),
            "podcast-stereo" => Some(Self {
                name: "podcast-stereo",
                target_lufs: -16.0,
                ceiling_db: -1.0,
                description: "common stereo podcast delivery reference",
            }),
            "podcast-mono" => Some(Self {
                name: "podcast-mono",
                target_lufs: -19.0,
                ceiling_db: -1.0,
                description: "common mono podcast delivery reference",
            }),
            "ebu-r128" => Some(Self {
                name: "ebu-r128",
                target_lufs: -23.0,
                ceiling_db: -1.0,
                description: "EBU R 128 programme loudness",
            }),
            "atsc-a85" => Some(Self {
                name: "atsc-a85",
                target_lufs: -24.0,
                ceiling_db: -2.0,
                description: "ATSC A/85 television delivery",
            }),
            "arib-tr-b32" => Some(Self {
                name: "arib-tr-b32",
                target_lufs: -24.0,
                ceiling_db: -1.0,
                description: "ARIB TR-B32 Japanese digital television delivery",
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
}
