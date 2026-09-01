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
}
