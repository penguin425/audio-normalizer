//! Bounded, deterministic discovery of audio files below a directory.

use std::path::{Path, PathBuf};

pub(crate) const MAX_FILES: usize = 100_000;
const MAX_DIRECTORY_ENTRIES: usize = 1_000_000;
pub(crate) const MAX_DIRECTORY_DEPTH: usize = 64;

/// Find supported audio files below `root` in deterministic path order.
///
/// `root` itself is resolved once, but symbolic links and Windows reparse
/// points encountered below it are never followed. Discovery is bounded to
/// 100,000 audio files, 1,000,000 directory entries, and 64 directory levels.
/// When `recursive` is false, only regular files directly inside `root` are
/// returned.
pub fn discover_audio_files(root: &Path, recursive: bool) -> Result<Vec<PathBuf>, String> {
    discover_audio_files_excluding(root, recursive, None, None)
}

pub(crate) fn discover_audio_files_excluding(
    root: &Path,
    recursive: bool,
    excluded_file: Option<&Path>,
    excluded_directory: Option<&Path>,
) -> Result<Vec<PathBuf>, String> {
    let root = std::fs::canonicalize(root).map_err(|error| {
        format!(
            "canonicalize audio discovery root {}: {error}",
            root.display()
        )
    })?;
    if !root.is_dir() {
        return Err(format!(
            "audio discovery root is not a directory: {}",
            root.display()
        ));
    }
    let excluded_file = excluded_file.map(path_for_comparison).transpose()?;
    let excluded_directory = excluded_directory.map(path_for_comparison).transpose()?;
    let mut scanner = Scanner {
        root: &root,
        recursive,
        excluded_file: excluded_file.as_deref(),
        excluded_directory: excluded_directory.as_deref(),
        visited_entries: 0,
        files: Vec::new(),
    };
    scanner.collect(&root, 0)?;
    scanner.files.sort();
    Ok(scanner.files)
}

struct Scanner<'a> {
    root: &'a Path,
    recursive: bool,
    excluded_file: Option<&'a Path>,
    excluded_directory: Option<&'a Path>,
    visited_entries: usize,
    files: Vec<PathBuf>,
}

impl Scanner<'_> {
    fn collect(&mut self, directory: &Path, depth: usize) -> Result<(), String> {
        if depth > MAX_DIRECTORY_DEPTH {
            return Err(format!(
                "audio discovery exceeds the {MAX_DIRECTORY_DEPTH}-directory-depth limit"
            ));
        }
        if directory != self.root {
            let metadata = std::fs::symlink_metadata(directory)
                .map_err(|error| format!("inspect {}: {error}", directory.display()))?;
            if !metadata.file_type().is_dir() || is_link_like(&metadata) {
                return Ok(());
            }
        }

        let mut entries = Vec::new();
        for entry in std::fs::read_dir(directory)
            .map_err(|error| format!("read {}: {error}", directory.display()))?
        {
            self.visited_entries = self
                .visited_entries
                .checked_add(1)
                .ok_or_else(|| "audio discovery directory-entry count overflow".to_string())?;
            if self.visited_entries > MAX_DIRECTORY_ENTRIES {
                return Err(format!(
                    "audio discovery exceeds the {MAX_DIRECTORY_ENTRIES}-directory-entry limit"
                ));
            }
            entries.push(entry.map_err(|error| format!("read {}: {error}", directory.display()))?);
        }
        entries.sort_by_key(|entry| entry.path());

        for entry in entries {
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|error| format!("inspect {}: {error}", path.display()))?;
            if is_link_like(&metadata) {
                continue;
            }
            if metadata.file_type().is_dir() {
                if !self.recursive || self.excluded_directory == Some(path.as_path()) {
                    continue;
                }
                let resolved = std::fs::canonicalize(&path)
                    .map_err(|error| format!("canonicalize {}: {error}", path.display()))?;
                if !resolved.starts_with(self.root) {
                    return Err(format!(
                        "audio discovery directory escaped its root: {}",
                        path.display()
                    ));
                }
                self.collect(&resolved, depth + 1)?;
            } else if metadata.file_type().is_file()
                && self.excluded_file != Some(path.as_path())
                && is_supported_audio_path(&path)
            {
                self.files.push(path);
                if self.files.len() > MAX_FILES {
                    return Err(format!(
                        "audio discovery exceeds the {MAX_FILES}-file limit"
                    ));
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn is_supported_audio_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some(
            "wav"
                | "wave"
                | "bwf"
                | "bw64"
                | "rf64"
                | "dsf"
                | "dff"
                | "mp3"
                | "flac"
                | "aac"
                | "m4a"
                | "mp4"
                | "ogg"
                | "opus"
        )
    )
}

fn path_for_comparison(path: &Path) -> Result<PathBuf, String> {
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let parent = std::fs::canonicalize(parent).map_err(|parent_error| {
                format!(
                    "canonicalize parent of excluded path {}: {parent_error}",
                    path.display()
                )
            })?;
            let name = path.file_name().ok_or_else(|| {
                format!("excluded path has no final component: {}", path.display())
            })?;
            Ok(parent.join(name))
        }
        Err(error) => Err(format!(
            "canonicalize excluded path {}: {error}",
            path.display()
        )),
    }
}

#[cfg(windows)]
fn is_link_like(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link_like(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_is_sorted_and_filters_extensions() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("z.WAV"), b"audio").unwrap();
        std::fs::write(directory.path().join("a.flac"), b"audio").unwrap();
        std::fs::write(directory.path().join("notes.txt"), b"text").unwrap();

        let files = discover_audio_files(directory.path(), false).unwrap();
        assert_eq!(
            files
                .iter()
                .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["a.flac", "z.WAV"]
        );
    }

    #[test]
    fn recursion_is_explicit_and_depth_is_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("child");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(child.join("tone.wav"), b"audio").unwrap();
        assert!(discover_audio_files(directory.path(), false)
            .unwrap()
            .is_empty());
        assert_eq!(
            discover_audio_files(directory.path(), true).unwrap().len(),
            1
        );

        let mut nested = child;
        for index in 0..=MAX_DIRECTORY_DEPTH {
            nested = nested.join(format!("d{index}"));
            std::fs::create_dir(&nested).unwrap();
        }
        assert!(discover_audio_files(directory.path(), true)
            .unwrap_err()
            .contains("directory-depth limit"));
    }

    #[cfg(unix)]
    #[test]
    fn links_below_the_explicit_root_are_not_followed() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        let outside = directory.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("tone.wav"), b"audio").unwrap();
        symlink(&outside, root.join("linked-directory")).unwrap();
        symlink(outside.join("tone.wav"), root.join("linked.wav")).unwrap();

        assert!(discover_audio_files(&root, true).unwrap().is_empty());
    }

    #[test]
    fn exclusions_apply_to_files_and_subtrees() {
        let directory = tempfile::tempdir().unwrap();
        let skipped = directory.path().join("output");
        let state = directory.path().join("state.wav");
        std::fs::create_dir(&skipped).unwrap();
        std::fs::write(skipped.join("render.wav"), b"audio").unwrap();
        std::fs::write(&state, b"state").unwrap();
        std::fs::write(directory.path().join("input.wav"), b"audio").unwrap();

        let files =
            discover_audio_files_excluding(directory.path(), true, Some(&state), Some(&skipped))
                .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file_name().unwrap(), "input.wav");
    }
}
