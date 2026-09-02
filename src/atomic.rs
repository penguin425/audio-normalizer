//! Transactional output files staged beside their final destination.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::{Builder, NamedTempFile};

/// A sibling temporary file that replaces its destination only after the
/// complete encode, metadata write, and optional verification have succeeded.
pub(crate) struct AtomicOutput {
    destination: PathBuf,
    temporary: NamedTempFile,
}

impl AtomicOutput {
    pub(crate) fn new(destination: &Path) -> Result<Self, String> {
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let suffix = destination
            .extension()
            .map(|extension| format!(".{}", extension.to_string_lossy()))
            .unwrap_or_default();
        let temporary = Builder::new()
            .prefix(".forge-")
            .suffix(&suffix)
            .tempfile_in(parent)
            .map_err(|error| {
                format!(
                    "create temporary output beside {}: {error}",
                    destination.display()
                )
            })?;
        Ok(Self {
            destination: destination.to_owned(),
            temporary,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        self.temporary.path()
    }

    pub(crate) fn file_mut(&mut self) -> &mut File {
        self.temporary.as_file_mut()
    }

    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.temporary
            .write_all(bytes)
            .map_err(|error| format!("write {}: {error}", self.temporary.path().display()))
    }

    pub(crate) fn copy_from_path(&mut self, source: &Path) -> Result<u64, String> {
        let mut input =
            File::open(source).map_err(|error| format!("open {}: {error}", source.display()))?;
        std::io::copy(&mut input, &mut self.temporary).map_err(|error| {
            format!(
                "copy {} into {}: {error}",
                source.display(),
                self.temporary.path().display()
            )
        })
    }

    pub(crate) fn commit(mut self) -> Result<(), String> {
        self.temporary
            .as_file_mut()
            .sync_all()
            .map_err(|error| format!("sync {}: {error}", self.temporary.path().display()))?;
        self.temporary.persist(&self.destination).map_err(|error| {
            format!(
                "commit output {}: {}",
                self.destination.display(),
                error.error
            )
        })?;
        Ok(())
    }

    /// Publishes the staged file only if the destination does not already
    /// exist.
    ///
    /// Unlike [`Self::commit`], this never replaces an existing destination,
    /// including one created after this `AtomicOutput` was staged. The
    /// destination creation is race-safe; on platforms without native
    /// no-replace rename support, `tempfile` may use a hard link followed by
    /// removal of the temporary name. If publishing fails, the temporary file
    /// is removed when the retained `NamedTempFile` in `PersistError` is
    /// dropped.
    pub(crate) fn commit_noclobber(mut self) -> Result<(), String> {
        self.temporary
            .as_file_mut()
            .sync_all()
            .map_err(|error| format!("sync {}: {error}", self.temporary.path().display()))?;
        self.temporary
            .persist_noclobber(&self.destination)
            .map_err(|error| {
                format!(
                    "commit output without replacing {}: {}",
                    self.destination.display(),
                    error.error
                )
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn dropping_uncommitted_output_preserves_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        std::fs::write(&destination, b"original").unwrap();
        {
            let mut output = AtomicOutput::new(&destination).unwrap();
            output.temporary.write_all(b"incomplete").unwrap();
        }
        assert_eq!(std::fs::read(&destination).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn commit_atomically_replaces_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        std::fs::write(&destination, b"original").unwrap();
        let mut output = AtomicOutput::new(&destination).unwrap();
        output.temporary.write_all(b"complete").unwrap();
        output.commit().unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"complete");
    }

    #[test]
    fn commit_noclobber_creates_absent_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        let mut output = AtomicOutput::new(&destination).unwrap();
        output.temporary.write_all(b"complete").unwrap();

        output.commit_noclobber().unwrap();

        assert_eq!(std::fs::read(&destination).unwrap(), b"complete");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn commit_noclobber_preserves_existing_destination_and_cleans_up() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        std::fs::write(&destination, b"original").unwrap();
        let mut output = AtomicOutput::new(&destination).unwrap();
        output.temporary.write_all(b"replacement").unwrap();

        let error = output.commit_noclobber().unwrap_err();

        assert!(error.contains("without replacing"));
        assert_eq!(std::fs::read(&destination).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn commit_noclobber_allows_only_one_staged_writer_to_win() {
        use std::sync::{Arc, Barrier};

        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        let mut first = AtomicOutput::new(&destination).unwrap();
        let mut second = AtomicOutput::new(&destination).unwrap();
        first.temporary.write_all(b"first").unwrap();
        second.temporary.write_all(b"second").unwrap();

        let barrier = Arc::new(Barrier::new(3));
        let first_barrier = Arc::clone(&barrier);
        let first_thread = std::thread::spawn(move || {
            first_barrier.wait();
            first.commit_noclobber()
        });
        let second_barrier = Arc::clone(&barrier);
        let second_thread = std::thread::spawn(move || {
            second_barrier.wait();
            second.commit_noclobber()
        });

        barrier.wait();
        let first_result = first_thread.join().unwrap();
        let second_result = second_thread.join().unwrap();

        assert_ne!(first_result.is_ok(), second_result.is_ok());
        let contents = std::fs::read(&destination).unwrap();
        assert!(contents == b"first" || contents == b"second");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
