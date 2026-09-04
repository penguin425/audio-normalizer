//! Transactional output files staged beside their final destination.

use crate::stable_input::identity_from_open_file;
use crate::stable_input::StableFileIdentity;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
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
    expected_destination: DestinationState,
    temporary: NamedTempFile,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DestinationState {
    Missing,
    Present {
        identity: StableFileIdentity,
        byte_len: u64,
        sha256: [u8; 32],
    },
}

impl AtomicOutput {
    pub(crate) fn new(destination: &Path) -> Result<Self, String> {
        Self::new_with_overwrite(destination, true)
    }

    pub(crate) fn new_with_overwrite(destination: &Path, overwrite: bool) -> Result<Self, String> {
        let expected_destination = DestinationState::capture(destination)?;
        if !overwrite && expected_destination != DestinationState::Missing {
            return Err(format!(
                "output already exists: {} (enable overwrite to replace it)",
                destination.display()
            ));
        }
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
            expected_destination,
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
        self.commit_open().map(drop)
    }

    pub(crate) fn commit_open(self) -> Result<File, String> {
        // Sync the owned inode first, then minimize (but cannot portably
        // eliminate) the pathname race by binding immediately before persist.
        // An intentional path-replacing rewrite must first be adopted.
        self.temporary
            .as_file()
            .sync_all()
            .map_err(|error| format!("sync {}: {error}", self.temporary.path().display()))?;
        let _bound_path_handle = self.bound_stage_file()?;
        let _bound_destination_handles = self
            .expected_destination
            .verify_immediately_before_commit(&self.destination)?;
        let destination = self.destination;
        let overwrite = matches!(self.expected_destination, DestinationState::Present { .. });
        let persisted = persist_temporary(self.temporary, &destination, overwrite)?;
        // The same open inode was synchronized immediately before the rename,
        // and no file data changes between that sync and publication. Syncing
        // it a second time here adds a full filesystem round trip without
        // strengthening durability. The parent-directory sync below makes the
        // rename durable on Unix; Windows uses MoveFileExW with WRITE_THROUGH.
        sync_parent_directory(&destination)?;
        Ok(persisted)
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

#[cfg(not(windows))]
fn persist_temporary(
    temporary: NamedTempFile,
    destination: &Path,
    overwrite: bool,
) -> Result<File, String> {
    if overwrite {
        temporary
            .persist(destination)
            .map_err(|error| format!("commit output {}: {}", destination.display(), error.error))
    } else {
        temporary.persist_noclobber(destination).map_err(|error| {
            format!(
                "commit output without overwrite {}: {}",
                destination.display(),
                error.error
            )
        })
    }
}

#[cfg(windows)]
fn persist_temporary(
    temporary: NamedTempFile,
    destination: &Path,
    overwrite: bool,
) -> Result<File, String> {
    use std::iter;
    use std::os::windows::ffi::OsStrExt;

    const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
    const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x0000_0100;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
        fn SetFileAttributesW(path: *const u16, attributes: u32) -> i32;
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(iter::once(0))
            .collect()
    }

    let source = temporary.path().to_owned();
    let source_wide = wide(&source);
    let destination_wide = wide(destination);
    // NamedTempFile marks named files as temporary on Windows. Clear that
    // attribute before publication, then request write-through rename
    // semantics so the directory update is not merely queued in memory.
    let normalized = unsafe { SetFileAttributesW(source_wide.as_ptr(), FILE_ATTRIBUTE_NORMAL) };
    if normalized == 0 {
        return Err(format!(
            "prepare Windows output {} for commit: {}",
            destination.display(),
            std::io::Error::last_os_error()
        ));
    }
    let flags = MOVEFILE_WRITE_THROUGH
        | if overwrite {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    let moved = unsafe { MoveFileExW(source_wide.as_ptr(), destination_wide.as_ptr(), flags) };
    if moved == 0 {
        let error = std::io::Error::last_os_error();
        let _ = unsafe { SetFileAttributesW(source_wide.as_ptr(), FILE_ATTRIBUTE_TEMPORARY) };
        let action = if overwrite {
            "commit output"
        } else {
            "commit output without overwrite"
        };
        return Err(format!("{action} {}: {error}", destination.display()));
    }

    let (file, old_path) = temporary.into_parts();
    drop(old_path);
    Ok(file)
}

impl DestinationState {
    fn capture(path: &Path) -> Result<Self, String> {
        let file = match open_regular_destination(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::Missing),
            Err(error) => {
                return Err(format!(
                    "inspect output destination {}: {error}",
                    path.display()
                ))
            }
        };
        let snapshot = snapshot_open_destination(&file, path)?;
        let confirmation = open_regular_destination(path).map_err(|error| {
            format!(
                "reopen output destination {} after hashing: {error}",
                path.display()
            )
        })?;
        let confirmation_identity = identity_from_open_file(&confirmation, path)
            .map_err(|error| format!("identify output destination {}: {error}", path.display()))?;
        if snapshot.identity() != &confirmation_identity {
            return Err(format!(
                "output destination changed while its initial state was captured: {}",
                path.display()
            ));
        }
        Ok(snapshot)
    }

    fn identity(&self) -> &StableFileIdentity {
        match self {
            Self::Present { identity, .. } => identity,
            Self::Missing => unreachable!("missing destinations have no file identity"),
        }
    }

    fn verify_immediately_before_commit(&self, path: &Path) -> Result<Vec<File>, String> {
        let Self::Present { .. } = self else {
            // persist_noclobber performs the missing-state comparison and the
            // publication as one filesystem operation.
            return Ok(Vec::new());
        };
        let current = open_regular_destination(path).map_err(|error| {
            format!(
                "output destination changed before commit {}: {error}",
                path.display()
            )
        })?;
        let observed = snapshot_open_destination(&current, path)?;
        if &observed != self {
            return Err(format!(
                "output destination changed after processing began; refusing to replace {}",
                path.display()
            ));
        }
        let confirmation = open_regular_destination(path).map_err(|error| {
            format!(
                "confirm output destination immediately before commit {}: {error}",
                path.display()
            )
        })?;
        let confirmation_identity = identity_from_open_file(&confirmation, path)
            .map_err(|error| format!("identify output destination {}: {error}", path.display()))?;
        if observed.identity() != &confirmation_identity {
            return Err(format!(
                "output destination changed immediately before commit: {}",
                path.display()
            ));
        }
        Ok(vec![current, confirmation])
    }
}

fn snapshot_open_destination(file: &File, path: &Path) -> Result<DestinationState, String> {
    let before = file.metadata().map_err(|error| {
        format!(
            "inspect opened output destination {}: {error}",
            path.display()
        )
    })?;
    if !before.is_file() {
        return Err(format!(
            "output destination is not a regular file: {}",
            path.display()
        ));
    }
    let identity = identity_from_open_file(file, path)
        .map_err(|error| format!("identify output destination {}: {error}", path.display()))?;
    let mut reader = file
        .try_clone()
        .map_err(|error| format!("clone output destination {}: {error}", path.display()))?;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek output destination {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| format!("hash output destination {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let after = file.metadata().map_err(|error| {
        format!(
            "reinspect opened output destination {}: {error}",
            path.display()
        )
    })?;
    if before.len() != after.len() {
        return Err(format!(
            "output destination length changed while it was hashed: {}",
            path.display()
        ));
    }
    Ok(DestinationState::Present {
        identity,
        byte_len: after.len(),
        sha256: hasher.finalize().into(),
    })
}

fn open_regular_destination(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(windows)]
    options
        .share_mode(0x0000_0001 | 0x0000_0002 | 0x0000_0004)
        .custom_flags(0x0020_0000);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(windows)]
    if metadata.file_attributes() & 0x0000_0400 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination is a reparse point",
        ));
    }
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn sync_parent_directory(destination: &Path) -> Result<(), String> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            format!(
                "sync output directory {} after committing {}: {error}",
                parent.display(),
                destination.display()
            )
        })
}

#[cfg(not(unix))]
fn sync_parent_directory(_destination: &Path) -> Result<(), String> {
    // Windows exposes durable move semantics through MoveFileEx rather than a
    // portable directory-fsync equivalent. The persisted file handle itself
    // is synchronized above; the publication primitive is strengthened on
    // Windows separately from this Unix directory step.
    Ok(())
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
    fn commit_never_clobbers_a_destination_created_during_staging() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        let mut output = AtomicOutput::new(&destination).unwrap();
        output.write_all(b"generated").unwrap();
        std::fs::write(&destination, b"competitor").unwrap();

        let error = output.commit().unwrap_err();
        assert!(error.contains("without overwrite"), "{error}");
        assert_eq!(std::fs::read(&destination).unwrap(), b"competitor");
    }

    #[test]
    fn commit_rejects_same_inode_same_length_destination_changes() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        std::fs::write(&destination, b"original").unwrap();
        let mut output = AtomicOutput::new(&destination).unwrap();
        output.write_all(b"generated").unwrap();
        std::fs::write(&destination, b"tampered").unwrap();

        let error = output.commit().unwrap_err();
        assert!(error.contains("changed after processing began"), "{error}");
        assert_eq!(std::fs::read(&destination).unwrap(), b"tampered");
    }

    #[test]
    fn commit_rejects_destination_inode_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        let displaced = directory.path().join("displaced.wav");
        std::fs::write(&destination, b"original").unwrap();
        let mut output = AtomicOutput::new(&destination).unwrap();
        output.write_all(b"generated").unwrap();
        std::fs::rename(&destination, &displaced).unwrap();
        std::fs::write(&destination, b"competitor").unwrap();

        let error = output.commit().unwrap_err();
        assert!(error.contains("changed after processing began"), "{error}");
        assert_eq!(std::fs::read(&destination).unwrap(), b"competitor");
        assert_eq!(std::fs::read(&displaced).unwrap(), b"original");
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
