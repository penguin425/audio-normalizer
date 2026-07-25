//! Cross-container audio metadata preservation.

use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::tag::{ItemKey, Tag, TagExt};
use std::path::Path;

/// Copy the source's primary metadata tag to the destination container.
///
/// Lofty's generic tag representation retains common text fields and artwork.
/// Remapping discards only fields that the destination tag format cannot
/// represent.
pub fn copy_metadata(input: &Path, output: &Path) -> Result<(), String> {
    let source = lofty::read_from_path(input)
        .map_err(|error| format!("read metadata {}: {error}", input.display()))?;
    let Some(mut tag) = source.primary_tag().or_else(|| source.first_tag()).cloned() else {
        return Ok(());
    };

    let destination = lofty::read_from_path(output)
        .map_err(|error| format!("read output metadata {}: {error}", output.display()))?;
    tag.re_map(destination.primary_tag_type());
    tag.save_to_path(output, WriteOptions::default())
        .map_err(|error| format!("write metadata {}: {error}", output.display()))
}

/// Write ReplayGain 2.0 fields while leaving encoded audio untouched.
///
/// ReplayGain 2.0 uses EBU R128 measurement with a -18 LUFS reference.
pub fn write_replaygain(
    path: &Path,
    track_lufs: f64,
    track_peak: f32,
    album: Option<(f64, f32)>,
) -> Result<(), String> {
    let tagged = lofty::read_from_path(path)
        .map_err(|error| format!("read metadata {}: {error}", path.display()))?;
    let mut tag = tagged
        .primary_tag()
        .or_else(|| tagged.first_tag())
        .cloned()
        .unwrap_or_else(|| Tag::new(tagged.primary_tag_type()));
    tag.re_map(tagged.primary_tag_type());
    tag.insert_text(
        ItemKey::ReplayGainTrackGain,
        format!("{:+.2} dB", -18.0 - track_lufs),
    );
    tag.insert_text(ItemKey::ReplayGainTrackPeak, format!("{:.8}", track_peak));
    if let Some((album_lufs, album_peak)) = album {
        tag.insert_text(
            ItemKey::ReplayGainAlbumGain,
            format!("{:+.2} dB", -18.0 - album_lufs),
        );
        tag.insert_text(ItemKey::ReplayGainAlbumPeak, format!("{:.8}", album_peak));
    }
    tag.save_to_path(path, WriteOptions::default())
        .map_err(|error| format!("write metadata {}: {error}", path.display()))
}
