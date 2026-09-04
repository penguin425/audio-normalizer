//! Transactional helpers for non-audio output files.

use crate::atomic::AtomicOutput;
use std::fs::File;
use std::path::Path;

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
    prepare_parent(path)?;
    let mut output = AtomicOutput::new_with_overwrite(path, overwrite)?;
    write(output.file_mut())?;
    output.commit()
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
}
