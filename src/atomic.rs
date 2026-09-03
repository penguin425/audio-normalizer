//! Transactional output files staged beside their final destination.

use crate::stable_input::identity_from_open_file;
use std::fs::{File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use tempfile::{Builder, NamedTempFile};

/// A sibling temporary file that replaces its destination only after the
/// complete encode, metadata write, and optional verification have succeeded.
///
/// Final-component links and accidental staging-path replacement are rejected
/// on the supported Unix and Windows targets. The containing output directory
/// must still be trusted against hostile concurrent renames: `tempfile` must
/// ultimately publish by pathname, and Rust has no portable rename-from-handle
/// primitive that could close that last pathname lookup window.
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

    /// Adopt the regular file currently named by the staging path after a
    /// trusted path-based writer has completed.
    ///
    /// Path-based container metadata writers can produce a complete sibling
    /// file and rename it over the staging path. `NamedTempFile` continues to
    /// hold the old inode in that case, so committing it would sync a file
    /// other than the one being published. Callers must explicitly adopt the
    /// replacement before committing; unexpected replacements are rejected by
    /// [`Self::commit`]. Calling this after an in-place writer is harmless and
    /// keeps the trust boundary explicit if that writer later changes its
    /// implementation to an atomic path replacement.
    pub(crate) fn adopt_path_writer_output(&mut self) -> Result<(), String> {
        let path = self.temporary.path().to_owned();
        let owned_identity = identity_from_open_file(self.temporary.as_file(), &path)
            .map_err(|error| format!("identify owned staging file {}: {error}", path.display()))?;
        let observed = open_regular_stage(&path, false)?;
        let observed_identity = identity_from_open_file(&observed, &path).map_err(|error| {
            format!("identify trusted writer output {}: {error}", path.display())
        })?;

        // Most metadata operations either make no change or update the
        // existing inode in place. Keep the caller-owned handle in that common
        // case: commit performs its own final path binding, and needlessly
        // reopening/rebinding every ordinary WAV/FLAC output adds measurable
        // fixed filesystem overhead to short jobs.
        if owned_identity == observed_identity {
            return Ok(());
        }

        let replacement = open_regular_stage(&path, true)?;
        let replacement_identity =
            identity_from_open_file(&replacement, &path).map_err(|error| {
                format!("identify adopted writer output {}: {error}", path.display())
            })?;
        if observed_identity != replacement_identity {
            return Err(format!(
                "staging path {} changed while adopting trusted writer output",
                path.display()
            ));
        }

        // Retain the TempPath's cleanup ownership while rebinding the file
        // handle to the replacement inode. A second identity check rejects a
        // further pathname change before returning to the trusted caller.
        *self.temporary.as_file_mut() = replacement;
        self.bound_stage_file().map(|_| ())
    }

    pub(crate) fn commit(self) -> Result<(), String> {
        // Sync the owned inode first, then minimize (but cannot portably
        // eliminate) the pathname race by binding immediately before persist.
        // An intentional path-replacing rewrite must first be adopted.
        self.temporary
            .as_file()
            .sync_all()
            .map_err(|error| format!("sync {}: {error}", self.temporary.path().display()))?;
        let _bound_path_handle = self.bound_stage_file()?;
        self.temporary.persist(&self.destination).map_err(|error| {
            format!(
                "commit output {}: {}",
                self.destination.display(),
                error.error
            )
        })?;
        Ok(())
    }

    #[cfg(unix)]
    fn bound_stage_file(&self) -> Result<File, String> {
        let path = self.temporary.path();
        // `chmod`-style permission preservation can intentionally remove
        // read access from a caller-owned temporary file. On Unix, pathname
        // metadata still exposes a no-follow device/inode binding without
        // reopening the contents. Keep a duplicate of the already-open owned
        // handle alive until persist so the check never adds a read-permission
        // requirement.
        let owned =
            self.temporary.as_file().metadata().map_err(|error| {
                format!("inspect owned staging file {}: {error}", path.display())
            })?;
        let current = std::fs::symlink_metadata(path)
            .map_err(|error| format!("inspect current staging path {}: {error}", path.display()))?;
        let confirmation = std::fs::symlink_metadata(path)
            .map_err(|error| format!("confirm current staging path {}: {error}", path.display()))?;
        if !current.file_type().is_file() || !confirmation.file_type().is_file() {
            return Err(format!(
                "refuse non-regular staging path {}",
                path.display()
            ));
        }
        let owned_identity = (owned.dev(), owned.ino());
        let current_identity = (current.dev(), current.ino());
        let confirmation_identity = (confirmation.dev(), confirmation.ino());
        if owned_identity != current_identity || current_identity != confirmation_identity {
            return Err(format!(
                "refuse to publish {}: staging path no longer identifies the owned file; \
                 explicitly adopt an intentional replacement before commit",
                self.destination.display()
            ));
        }
        self.temporary
            .as_file()
            .try_clone()
            .map_err(|error| format!("retain owned staging file {}: {error}", path.display()))
    }

    #[cfg(windows)]
    fn bound_stage_file(&self) -> Result<File, String> {
        let path = self.temporary.path();
        // Read-only Windows output attributes must not make publication fail.
        // An attribute-only handle supplies the same stable volume/file index
        // while refusing reparse points and allowing the final rename.
        let current = open_regular_stage_attributes(path)?;
        let confirmation = open_regular_stage_attributes(path)?;
        let owned_identity = identity_from_open_file(self.temporary.as_file(), path)
            .map_err(|error| format!("identify owned staging file {}: {error}", path.display()))?;
        let current_identity = identity_from_open_file(&current, path).map_err(|error| {
            format!("identify current staging file {}: {error}", path.display())
        })?;
        let confirmation_identity = identity_from_open_file(&confirmation, path)
            .map_err(|error| format!("confirm current staging file {}: {error}", path.display()))?;
        if owned_identity != current_identity || current_identity != confirmation_identity {
            return Err(format!(
                "refuse to publish {}: staging path no longer identifies the owned file; \
                 explicitly adopt an intentional replacement before commit",
                self.destination.display()
            ));
        }
        Ok(confirmation)
    }

    #[cfg(not(any(unix, windows)))]
    fn bound_stage_file(&self) -> Result<File, String> {
        let path = self.temporary.path();
        let current = open_regular_stage(path, false)?;
        let owned_identity = identity_from_open_file(self.temporary.as_file(), path)
            .map_err(|error| format!("identify owned staging file {}: {error}", path.display()))?;
        let current_identity = identity_from_open_file(&current, path).map_err(|error| {
            format!("identify current staging file {}: {error}", path.display())
        })?;
        if owned_identity != current_identity {
            return Err(format!(
                "refuse to publish {}: staging path no longer identifies the owned file; \
                 explicitly adopt an intentional replacement before commit",
                self.destination.display()
            ));
        }
        Ok(current)
    }
}

#[cfg(windows)]
fn open_regular_stage_attributes(path: &Path) -> Result<File, String> {
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    let mut options = OpenOptions::new();
    options
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options
        .open(path)
        .map_err(|error| format!("open staging path attributes {}: {error}", path.display()))?;
    let metadata = file.metadata().map_err(|error| {
        format!(
            "inspect opened staging path attributes {}: {error}",
            path.display()
        )
    })?;
    if metadata.file_attributes() & 0x0000_0400 != 0 {
        return Err(format!(
            "refuse reparse-point staging path {}",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!(
            "refuse non-regular staging path {}",
            path.display()
        ));
    }
    Ok(file)
}

fn open_regular_stage(path: &Path, writable: bool) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(writable);
    // Refuse a final-component symlink and avoid blocking if an attacker swaps
    // in a FIFO between trusted writer completion and this open.
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    // Open a reparse point itself so the handle-based attribute check below
    // can reject it instead of silently following it.
    #[cfg(windows)]
    options.custom_flags(0x0020_0000);

    let file = options
        .open(path)
        .map_err(|error| format!("open staging path {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect opened staging path {}: {error}", path.display()))?;
    #[cfg(windows)]
    if metadata.file_attributes() & 0x0000_0400 != 0 {
        return Err(format!(
            "refuse reparse-point staging path {}",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!(
            "refuse non-regular staging path {}",
            path.display()
        ));
    }
    Ok(file)
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
    fn commit_rejects_unadopted_stage_replacement_and_preserves_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        std::fs::write(&destination, b"original destination").unwrap();
        let mut output = AtomicOutput::new(&destination).unwrap();
        output.write_all(b"obsolete stage inode").unwrap();
        let stage_path = output.path().to_owned();

        let mut replacement = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        replacement.write_all(b"replacement stage inode").unwrap();
        replacement.persist(output.path()).unwrap();

        let error = output.commit().unwrap_err();
        assert!(error.contains("staging path no longer identifies the owned file"));
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"original destination"
        );
        assert!(!stage_path.exists());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn explicitly_adopted_stage_replacement_is_published() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        std::fs::write(&destination, b"original destination").unwrap();
        let mut output = AtomicOutput::new(&destination).unwrap();
        output.write_all(b"obsolete stage inode").unwrap();

        let mut replacement = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        replacement.write_all(b"replacement stage inode").unwrap();
        replacement.persist(output.path()).unwrap();

        output.adopt_path_writer_output().unwrap();
        output.commit().unwrap();
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"replacement stage inode"
        );
    }

    #[cfg(unix)]
    #[test]
    fn commit_does_not_require_staged_content_read_permission() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        let mut output = AtomicOutput::new(&destination).unwrap();
        output.write_all(b"write-only result").unwrap();
        std::fs::set_permissions(output.path(), std::fs::Permissions::from_mode(0o200)).unwrap();

        output.commit().unwrap();

        assert_eq!(
            std::fs::metadata(&destination).unwrap().mode() & 0o777,
            0o200
        );
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"write-only result");
    }

    #[cfg(unix)]
    #[test]
    fn adoption_rejects_a_final_component_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        let replacement = directory.path().join("replacement.wav");
        std::fs::write(&replacement, b"replacement").unwrap();
        let mut output = AtomicOutput::new(&destination).unwrap();
        let stage_path = output.path().to_owned();
        std::fs::remove_file(&stage_path).unwrap();
        symlink(&replacement, &stage_path).unwrap();

        let error = output.adopt_path_writer_output().unwrap_err();
        assert!(error.contains("open staging path"), "{error}");
        assert_eq!(std::fs::read(&replacement).unwrap(), b"replacement");
        assert!(!destination.exists());
    }
}
