//! Transactional helpers for non-audio output files.

use crate::atomic::AtomicOutput;
use std::fs::File;
use std::path::Path;

/// A complete, synchronized non-audio file awaiting atomic publication.
///
/// Dropping this value removes its private sibling stage. Publication still
/// applies the no-clobber or unchanged-destination comparison captured when
/// the stage was created.
pub struct StagedFileOutput {
    output: AtomicOutput,
}

impl StagedFileOutput {
    /// Path of the private synchronized stage.
    pub fn staged_path(&self) -> &Path {
        self.output.path()
    }

    /// Atomically publish the staged file.
    pub fn commit(self) -> Result<(), String> {
        self.output.commit()
    }
}

/// Atomically publish an empty regular file and return its open handle.
///
/// This is intended for live streams such as progress logs whose pathname
/// must become visible before all content exists. Initial publication follows
/// the same no-clobber or unchanged-destination policy as
/// [`write_file_atomically`]. Writes made through the returned handle are live
/// and are not rolled back if the caller later fails.
pub fn create_live_file_atomically(path: &Path, overwrite: bool) -> Result<File, String> {
    prepare_parent(path)?;
    AtomicOutput::new_with_overwrite(path, overwrite)?.commit_open()
}

/// Write a complete file beside its destination, then publish it atomically.
///
/// If `overwrite` is false, publication uses an atomic no-clobber operation;
/// a destination created by another process while `write` is running is never
/// replaced. If `overwrite` is true and the destination already exists, its
/// file identity, length, and SHA-256 content must still match the state seen
/// before `write` started. The staged file and, on Unix, the containing
/// directory are synchronized before this function returns successfully.
pub fn write_file_atomically(
    path: &Path,
    overwrite: bool,
    write: impl FnOnce(&mut File) -> Result<(), String>,
) -> Result<(), String> {
    stage_file_atomically(path, overwrite, write)?.commit()
}

/// Write and synchronize a complete sibling file without publishing it yet.
///
/// This lets a caller finish auxiliary evidence before committing a primary
/// output. It does not make publication of multiple destination paths one
/// filesystem transaction; callers must document and recover any gap between
/// their ordered commits.
pub fn stage_file_atomically(
    path: &Path,
    overwrite: bool,
    write: impl FnOnce(&mut File) -> Result<(), String>,
) -> Result<StagedFileOutput, String> {
    prepare_parent(path)?;
    let mut output = AtomicOutput::new_with_overwrite(path, overwrite)?;
    write(output.file_mut())?;
    output.sync_stage()?;
    Ok(StagedFileOutput { output })
}

fn prepare_parent(path: &Path) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn writer_respects_overwrite_policy() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.json");
        std::fs::write(&path, b"old").unwrap();
        assert!(write_file_atomically(&path, false, |file| {
            file.write_all(b"new").map_err(|error| error.to_string())
        })
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"old");

        write_file_atomically(&path, true, |file| {
            file.write_all(b"new").map_err(|error| error.to_string())
        })
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn live_writer_publishes_and_retains_the_committed_handle() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("progress.ndjson");

        let mut file = create_live_file_atomically(&path, false).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        file.write_all(b"{\"event\":\"started\"}\n").unwrap();
        file.flush().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"event\":\"started\"}\n");

        assert!(create_live_file_atomically(&path, false).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"event\":\"started\"}\n");
    }

    #[test]
    fn staged_writer_is_private_until_commit_and_cleans_up_on_drop() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.json");
        let stage = stage_file_atomically(&path, false, |file| {
            file.write_all(b"evidence")
                .map_err(|error| error.to_string())
        })
        .unwrap();
        let staged_path = stage.staged_path().to_owned();
        assert!(!path.exists());
        assert_eq!(std::fs::read(&staged_path).unwrap(), b"evidence");
        drop(stage);
        assert!(!path.exists());
        assert!(!staged_path.exists());

        let stage = stage_file_atomically(&path, false, |file| {
            file.write_all(b"published")
                .map_err(|error| error.to_string())
        })
        .unwrap();
        stage.commit().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"published");
    }
}
