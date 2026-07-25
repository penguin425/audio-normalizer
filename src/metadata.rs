//! Cross-container audio metadata preservation.

use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::tag::TagExt;
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
