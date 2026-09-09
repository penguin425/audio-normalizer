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
pub(crate) enum DestinationPreimage {
    Missing,
    Present {
        identity: StableFileIdentity,
        byte_len: u64,
        sha256: [u8; 32],
    },
}

type DestinationState = DestinationPreimage;

impl AtomicOutput {
    pub(crate) fn new(destination: &Path) -> Result<Self, String> {
        Self::new_with_overwrite(destination, true)
    }

    pub(crate) fn new_with_overwrite(destination: &Path, overwrite: bool) -> Result<Self, String> {
        Self::new_with_overwrite_and_limit(destination, overwrite, u64::MAX)
    }

    /// Create an atomic output while bounding the bytes read to capture an
    /// existing destination preimage.
    pub(crate) fn new_with_overwrite_and_limit(
        destination: &Path,
        overwrite: bool,
        maximum_destination_bytes: u64,
    ) -> Result<Self, String> {
        let expected_destination =
            DestinationState::capture(destination, maximum_destination_bytes)?;
        Self::from_expected_destination(destination, overwrite, expected_destination)
    }

    /// Capture the exact destination state for a caller that has already
    /// checkpointed an output decision. The returned token includes the file
    /// identity, byte length, and complete content digest; a missing path is
    /// represented explicitly so a later creator is also detected.
    pub(crate) fn capture_destination_preimage(
        destination: &Path,
    ) -> Result<DestinationPreimage, String> {
        DestinationState::capture(destination, u64::MAX)
    }

    /// Create an atomic output against a previously captured destination
    /// preimage. Capturing again here closes the interval between a watch
    /// checkpoint and construction of the staging file; commit performs the
    /// existing final CAS check as well.
    pub(crate) fn new_with_overwrite_and_preimage(
        destination: &Path,
        overwrite: bool,
        expected_destination: DestinationPreimage,
    ) -> Result<Self, String> {
        let maximum_destination_bytes = expected_destination.maximum_capture_bytes();
        let observed = DestinationState::capture(destination, maximum_destination_bytes)?;
        if observed != expected_destination {
            return Err(format!(
                "output destination preimage changed before staging: {}",
                destination.display()
            ));
        }
        Self::from_expected_destination(destination, overwrite, expected_destination)
    }

    fn from_expected_destination(
        destination: &Path,
        overwrite: bool,
        expected_destination: DestinationState,
    ) -> Result<Self, String> {
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

    /// Re-open a stage that was deliberately retained by a restartable
    /// transaction.  The expected destination state is supplied by the
    /// transaction journal; the current pathname is captured and must match
    /// it before the returned value can be committed.
    pub(crate) fn from_existing_stage(
        destination: &Path,
        stage: &Path,
        expected_identity: StableFileIdentity,
        expected_byte_len: u64,
        expected_sha256: [u8; 32],
    ) -> Result<Self, String> {
        let expected_destination = DestinationState::capture(destination, expected_byte_len)?;
        let DestinationState::Present {
            identity,
            byte_len,
            sha256,
        } = &expected_destination
        else {
            return Err(format!(
                "metadata transaction destination disappeared: {}",
                destination.display()
            ));
        };
        if *identity != expected_identity
            || *byte_len != expected_byte_len
            || *sha256 != expected_sha256
        {
            return Err(format!(
                "metadata transaction destination preimage changed: {}",
                destination.display()
            ));
        }

        let file = open_regular_stage(stage, false)?;
        let path = tempfile::TempPath::try_from_path(stage).map_err(|error| {
            format!(
                "retain metadata transaction stage {}: {error}",
                stage.display()
            )
        })?;
        let mut temporary = tempfile::NamedTempFile::from_parts(file, path);
        // A ready stage is owned by the journal until publication succeeds.
        // Keeping cleanup disabled means a commit-time conflict leaves the
        // exact bytes available for a later resume instead of silently
        // converting the job back into an untracked partial operation.
        temporary.disable_cleanup(true);
        let output = Self {
            destination: destination.to_owned(),
            expected_destination,
            temporary,
        };
        output.bound_stage_file().map(drop)?;
        Ok(output)
    }

    pub(crate) fn path(&self) -> &Path {
        self.temporary.path()
    }

    /// Destination pathname captured by this atomic output.
    pub(crate) fn destination_path(&self) -> &Path {
        &self.destination
    }

    /// Destination preimage captured when this output was staged.
    ///
    /// Generation-level publication performs the destination check before it
    /// moves an existing destination into its private backup.  The generation
    /// journal needs the same token so that the check is not reimplemented by
    /// a caller with weaker identity semantics.
    pub(crate) fn generation_expected_destination(&self) -> &DestinationPreimage {
        &self.expected_destination
    }

    /// Verify the destination preimage immediately before a generation starts
    /// moving any destination.  This deliberately does not publish anything.
    pub(crate) fn generation_verify_destination(&self) -> Result<(), String> {
        self.expected_destination
            .verify_immediately_before_commit(&self.destination)
            .map(drop)
    }

    /// Verify that the owned stage pathname still names the inode held by this
    /// output.  A generation coordinator calls this before recording durable
    /// stage evidence and again before publication.
    pub(crate) fn generation_verify_stage(&self) -> Result<(), String> {
        self.bound_stage_file().map(drop)
    }

    /// Publish a generation member after its coordinator has moved the
    /// original destination to a private backup.  The no-clobber operation is
    /// intentional: a creator racing the backup window must make the whole
    /// generation fail and roll back rather than be overwritten.
    pub(crate) fn generation_publish(mut self) -> Result<File, String> {
        self.temporary
            .as_file()
            .sync_all()
            .map_err(|error| format!("sync {}: {error}", self.temporary.path().display()))?;
        let bound_path_handle = self.bound_stage_file()?;
        reject_generation_hardlink(&bound_path_handle, &self.destination)?;
        let source = self.temporary.path().to_owned();
        let destination = self.destination;
        // A ready generation journal, rather than this in-memory value, owns
        // the stage.  Keep it available for rollback if the no-clobber rename
        // fails after the original destination has already been backed up.
        self.temporary.disable_cleanup(true);
        move_path_without_replacing(&source, &destination)?;
        Ok(bound_path_handle)
    }

    /// Re-open a retained stage using an arbitrary destination preimage.  The
    /// older metadata-transaction bridge only accepted a present destination;
    /// generation jobs also need to resume stages whose original destination
    /// was missing.
    pub(crate) fn from_existing_stage_with_destination_preimage(
        destination: &Path,
        stage: &Path,
        expected_destination: DestinationPreimage,
    ) -> Result<Self, String> {
        let maximum_destination_bytes = expected_destination.maximum_capture_bytes();
        let observed = DestinationState::capture(destination, maximum_destination_bytes)?;
        if observed != expected_destination {
            return Err(format!(
                "generation destination preimage changed while reopening stage: {}",
                destination.display()
            ));
        }

        let file = open_regular_stage(stage, false)?;
        let path = tempfile::TempPath::try_from_path(stage)
            .map_err(|error| format!("retain generation stage {}: {error}", stage.display()))?;
        let mut temporary = tempfile::NamedTempFile::from_parts(file, path);
        temporary.disable_cleanup(true);
        let output = Self {
            destination: destination.to_owned(),
            expected_destination,
            temporary,
        };
        output.generation_verify_stage()?;
        Ok(output)
    }

    /// Synchronize the parent directory after a generation backup/restore.
    pub(crate) fn sync_generation_parent(path: &Path) -> Result<(), String> {
        sync_parent_directory(path)
    }

    /// Move a regular private file to an absent sibling without replacing a
    /// competing pathname.  The source identity is rebound immediately before
    /// the move and must match the evidence captured by the coordinator.
    pub(crate) fn generation_move_without_replacing(
        source: &Path,
        destination: &Path,
        expected_identity: &StableFileIdentity,
    ) -> Result<(), String> {
        let source_file = open_regular_stage(source, false)?;
        let observed_identity = identity_from_open_file(&source_file, source).map_err(|error| {
            format!(
                "identify generation move source {}: {error}",
                source.display()
            )
        })?;
        if &observed_identity != expected_identity {
            return Err(format!(
                "generation move source changed immediately before publication: {}",
                source.display()
            ));
        }
        reject_generation_hardlink(&source_file, source)?;
        match std::fs::symlink_metadata(destination) {
            Ok(_) => {
                return Err(format!(
                    "generation move destination already exists: {}",
                    destination.display()
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "inspect generation move destination {}: {error}",
                    destination.display()
                ))
            }
        }
        move_path_without_replacing(source, destination)
    }

    /// Keep the stage pathname after this value is dropped.  Restartable
    /// callers invoke this only after the stage has been fully verified and
    /// are responsible for recording the path in a durable journal.
    pub(crate) fn retain_stage(&mut self) {
        self.temporary.disable_cleanup(true);
    }

    pub(crate) fn file_mut(&mut self) -> &mut File {
        self.temporary.as_file_mut()
    }

    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.temporary
            .write_all(bytes)
            .map_err(|error| format!("write {}: {error}", self.temporary.path().display()))
    }

    /// Synchronize the owned staging inode without publishing it.
    pub(crate) fn sync_stage(&self) -> Result<(), String> {
        self.temporary
            .as_file()
            .sync_all()
            .map_err(|error| format!("sync {}: {error}", self.temporary.path().display()))
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

        let replacement = open_regular_stage(&path, false)?;
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

    /// Publish after one caller-owned destination check performed immediately
    /// after the built-in identity/content comparison and before rename.
    pub(crate) fn commit_with_destination_check(
        self,
        check: impl FnOnce(&Path) -> Result<(), String>,
    ) -> Result<(), String> {
        self.commit_open_with_destination_check(check).map(drop)
    }

    pub(crate) fn commit_open(self) -> Result<File, String> {
        self.commit_open_with_destination_check(|_| Ok(()))
    }

    fn commit_open_with_destination_check(
        self,
        check: impl FnOnce(&Path) -> Result<(), String>,
    ) -> Result<File, String> {
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
        check(&destination)?;
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

#[cfg(any(target_os = "linux", target_os = "android"))]
fn move_path_without_replacing(source: &Path, destination: &Path) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source_bytes = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        format!(
            "generation move source contains an interior NUL: {}",
            source.display()
        )
    })?;
    let destination_bytes = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        format!(
            "generation move destination contains an interior NUL: {}",
            destination.display()
        )
    })?;
    // Call the kernel ABI directly.  Referencing the libc `renameat2` symbol
    // would raise the runtime glibc floor to 2.28 (and the Android API floor to
    // the Bionic version that first exported it), even though the underlying
    // syscall is available on older supported kernels.  ENOSYS still fails
    // closed without replacing the destination.
    let renamed = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source_bytes.as_ptr(),
            libc::AT_FDCWD,
            destination_bytes.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if renamed != 0 {
        return Err(format!(
            "rename generation path {} to {} without replacing: {}",
            source.display(),
            destination.display(),
            std::io::Error::last_os_error()
        ));
    }
    sync_parent_directory(destination)
}

#[cfg(windows)]
fn windows_wide_path(path: &Path) -> Result<Vec<u16>, String> {
    use std::os::windows::ffi::OsStrExt;

    let mut value = Vec::new();
    for unit in path.as_os_str().encode_wide() {
        if unit == 0 {
            return Err(format!(
                "Windows path contains an interior NUL: {}",
                path.display()
            ));
        }
        value.push(unit);
    }
    value.push(0);
    Ok(value)
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos"
))]
fn move_path_without_replacing(source: &Path, destination: &Path) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source_bytes = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        format!(
            "generation move source contains an interior NUL: {}",
            source.display()
        )
    })?;
    let destination_bytes = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        format!(
            "generation move destination contains an interior NUL: {}",
            destination.display()
        )
    })?;
    let renamed = unsafe {
        libc::renamex_np(
            source_bytes.as_ptr(),
            destination_bytes.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if renamed != 0 {
        return Err(format!(
            "rename generation path {} to {} without replacing: {}",
            source.display(),
            destination.display(),
            std::io::Error::last_os_error()
        ));
    }
    sync_parent_directory(destination)
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos"
    ))
))]
fn move_path_without_replacing(source: &Path, destination: &Path) -> Result<(), String> {
    let _ = (source, destination);
    Err("generation atomic no-clobber rename is unsupported on this Unix target".into())
}

fn reject_generation_hardlink(file: &File, path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    if file
        .metadata()
        .map_err(|error| format!("inspect generation link count {}: {error}", path.display()))?
        .nlink()
        > 1
    {
        return Err(format!(
            "generation private file must not have hard-link aliases: {}",
            path.display()
        ));
    }
    #[cfg(windows)]
    if crate::stable_input::windows_file_link_count(file)
        .map_err(|error| format!("inspect generation link count {}: {error}", path.display()))?
        > 1
    {
        return Err(format!(
            "generation private file must not have hard-link aliases: {}",
            path.display()
        ));
    }
    #[cfg(not(any(unix, windows)))]
    let _ = (file, path);
    Ok(())
}

#[cfg(windows)]
fn move_path_without_replacing(source: &Path, destination: &Path) -> Result<(), String> {
    const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x0000_0100;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
        fn GetFileAttributesW(path: *const u16) -> u32;
        fn SetFileAttributesW(path: *const u16, attributes: u32) -> i32;
    }

    let source_wide = windows_wide_path(source)?;
    let destination_wide = windows_wide_path(destination)?;
    // Only clear the temporary bit used by NamedTempFile; preserve any
    // caller-owned read-only/hidden attributes on a backup or returned stage.
    let attributes = unsafe { GetFileAttributesW(source_wide.as_ptr()) };
    if attributes == u32::MAX {
        return Err(format!(
            "inspect generation move source {}: {}",
            source.display(),
            std::io::Error::last_os_error()
        ));
    }
    let had_temporary_attribute = attributes & FILE_ATTRIBUTE_TEMPORARY != 0;
    if had_temporary_attribute {
        let normalized = unsafe {
            SetFileAttributesW(source_wide.as_ptr(), attributes & !FILE_ATTRIBUTE_TEMPORARY)
        };
        if normalized == 0 {
            return Err(format!(
                "prepare generation move source {}: {}",
                source.display(),
                std::io::Error::last_os_error()
            ));
        }
    }
    let moved = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        let error = std::io::Error::last_os_error();
        if had_temporary_attribute {
            let restored = unsafe { SetFileAttributesW(source_wide.as_ptr(), attributes) };
            if restored == 0 {
                let restore_error = std::io::Error::last_os_error();
                return Err(format!(
                    "move generation path {} to {} without replacing: {error}; "
                        + "restore source attributes: {restore_error}",
                    source.display(),
                    destination.display()
                ));
            }
        }
        return Err(format!(
            "move generation path {} to {} without replacing: {error}",
            source.display(),
            destination.display()
        ));
    }
    sync_parent_directory(destination)
}

#[cfg(not(any(unix, windows)))]
fn move_path_without_replacing(source: &Path, destination: &Path) -> Result<(), String> {
    // Unknown targets have no portable no-clobber primitive.  Refuse the
    // operation rather than silently turning the generation into an overwrite.
    let _ = (source, destination);
    Err("generation no-clobber move is unsupported on this target".into())
}

#[cfg(windows)]
fn persist_temporary(
    temporary: NamedTempFile,
    destination: &Path,
    overwrite: bool,
) -> Result<File, String> {
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
    const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x0000_0100;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
        fn GetFileAttributesW(path: *const u16) -> u32;
        fn SetFileAttributesW(path: *const u16, attributes: u32) -> i32;
    }

    let source = temporary.path().to_owned();
    let source_wide = windows_wide_path(&source)?;
    let destination_wide = windows_wide_path(destination)?;
    // NamedTempFile marks named files as temporary on Windows. Clear only that
    // bit before publication; a trusted writer may also have applied hidden,
    // read-only, archive, or other attributes that must survive the move.
    let attributes = unsafe { GetFileAttributesW(source_wide.as_ptr()) };
    if attributes == u32::MAX {
        return Err(format!(
            "inspect Windows output stage for {}: {}",
            destination.display(),
            std::io::Error::last_os_error()
        ));
    }
    let had_temporary_attribute = attributes & FILE_ATTRIBUTE_TEMPORARY != 0;
    if had_temporary_attribute {
        let without_temporary = attributes & !FILE_ATTRIBUTE_TEMPORARY;
        let normalized_attributes = if without_temporary == 0 {
            FILE_ATTRIBUTE_NORMAL
        } else {
            without_temporary
        };
        let normalized = unsafe { SetFileAttributesW(source_wide.as_ptr(), normalized_attributes) };
        if normalized == 0 {
            return Err(format!(
                "prepare Windows output {} for commit: {}",
                destination.display(),
                std::io::Error::last_os_error()
            ));
        }
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
        let action = if overwrite {
            "commit output"
        } else {
            "commit output without overwrite"
        };
        if had_temporary_attribute {
            let restored = unsafe { SetFileAttributesW(source_wide.as_ptr(), attributes) };
            if restored == 0 {
                return Err(format!(
                    "{action} {}: {error}; restore stage attributes: {}",
                    destination.display(),
                    std::io::Error::last_os_error()
                ));
            }
        }
        return Err(format!("{action} {}: {error}", destination.display()));
    }

    let (file, old_path) = temporary.into_parts();
    drop(old_path);
    Ok(file)
}

impl DestinationState {
    fn maximum_capture_bytes(&self) -> u64 {
        match self {
            Self::Missing => u64::MAX,
            Self::Present { byte_len, .. } => *byte_len,
        }
    }

    pub(crate) fn sha256_hex(&self) -> Option<String> {
        match self {
            Self::Missing => None,
            Self::Present { sha256, .. } => {
                let mut text = String::with_capacity(sha256.len() * 2);
                for byte in sha256 {
                    use std::fmt::Write as _;
                    write!(&mut text, "{byte:02x}").expect("writing to String cannot fail");
                }
                Some(text)
            }
        }
    }

    /// Return the identity, byte length, and digest of a present preimage.
    /// `None` represents an explicitly missing destination.
    pub(crate) fn generation_present_parts(&self) -> Option<(&StableFileIdentity, u64, &[u8; 32])> {
        match self {
            Self::Missing => None,
            Self::Present {
                identity,
                byte_len,
                sha256,
            } => Some((identity, *byte_len, sha256)),
        }
    }

    fn capture(path: &Path, maximum_bytes: u64) -> Result<Self, String> {
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
        let snapshot = snapshot_open_destination(&file, path, maximum_bytes)?;
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
        let maximum_bytes = match self {
            Self::Present { byte_len, .. } => *byte_len,
            Self::Missing => unreachable!("missing destination returned above"),
        };
        let observed = snapshot_open_destination(&current, path, maximum_bytes)?;
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

fn snapshot_open_destination(
    file: &File,
    path: &Path,
    maximum_bytes: u64,
) -> Result<DestinationState, String> {
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
    if before.len() > maximum_bytes {
        return Err(format!(
            "output destination exceeds the configured {maximum_bytes}-byte bound: {}",
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
    options
        .share_mode(0x0000_0001 | 0x0000_0002 | 0x0000_0004)
        .custom_flags(0x0020_0000);

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

    #[cfg(windows)]
    #[test]
    fn windows_wide_path_rejects_interior_nul() {
        let error = windows_wide_path(Path::new("cache\0entry")).unwrap_err();
        assert!(error.contains("interior NUL"), "{error}");
    }

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
    fn destination_preimage_capture_is_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        std::fs::write(&destination, b"destination bytes").unwrap();

        let result = AtomicOutput::new_with_overwrite_and_limit(&destination, true, 4);
        assert!(result.is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"destination bytes");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn checkpointed_destination_preimage_rejects_replacement_before_staging() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        let displaced = directory.path().join("displaced.wav");
        std::fs::write(&destination, b"checkpointed destination").unwrap();
        let preimage = AtomicOutput::capture_destination_preimage(&destination).unwrap();
        std::fs::rename(&destination, &displaced).unwrap();
        std::fs::write(&destination, b"competitor destination").unwrap();

        let error = AtomicOutput::new_with_overwrite_and_preimage(&destination, true, preimage)
            .err()
            .expect("replacement must be rejected before staging");
        assert!(error.contains("preimage changed before staging"), "{error}");
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"competitor destination"
        );
        assert_eq!(
            std::fs::read(&displaced).unwrap(),
            b"checkpointed destination"
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn checkpointed_destination_preimage_is_checked_again_at_commit() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        std::fs::write(&destination, b"checkpointed destination").unwrap();
        let preimage = AtomicOutput::capture_destination_preimage(&destination).unwrap();
        let mut output =
            AtomicOutput::new_with_overwrite_and_preimage(&destination, true, preimage).unwrap();
        output.write_all(b"generated output").unwrap();
        std::fs::write(&destination, b"competitor destination").unwrap();

        let error = output.commit().unwrap_err();
        assert!(error.contains("changed after processing began"), "{error}");
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"competitor destination"
        );
    }

    #[test]
    fn checkpointed_missing_destination_rejects_a_racing_creator() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        let preimage = AtomicOutput::capture_destination_preimage(&destination).unwrap();
        std::fs::write(&destination, b"competitor destination").unwrap();

        let error = AtomicOutput::new_with_overwrite_and_preimage(&destination, false, preimage)
            .err()
            .expect("racing creator must be rejected before staging");
        assert!(error.contains("preimage changed before staging"), "{error}");
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"competitor destination"
        );
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
        std::fs::write(&destination, b"rivalxxx").unwrap();

        let error = output.commit().unwrap_err();
        assert!(error.contains("changed after processing began"), "{error}");
        assert_eq!(std::fs::read(&destination).unwrap(), b"rivalxxx");
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
    fn read_only_path_replacement_is_adopted() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        std::fs::write(&destination, b"original destination").unwrap();
        let mut output = AtomicOutput::new(&destination).unwrap();
        output.write_all(b"obsolete stage inode").unwrap();

        let mut replacement = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        replacement.write_all(b"read-only replacement").unwrap();
        replacement.persist(output.path()).unwrap();
        std::fs::set_permissions(output.path(), std::fs::Permissions::from_mode(0o400)).unwrap();

        output.adopt_path_writer_output().unwrap();
        output.commit().unwrap();
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"read-only replacement"
        );
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn destination_hardlink_check_runs_after_preflight() {
        use std::os::unix::fs::MetadataExt;

        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        let alias = directory.path().join("result-alias.wav");
        std::fs::write(&destination, b"original destination").unwrap();
        let mut output = AtomicOutput::new(&destination).unwrap();
        output.write_all(b"generated destination").unwrap();

        let result = output.commit_with_destination_check(|path| {
            // This mutation occurs after AtomicOutput has completed its own
            // preimage comparison and immediately before persist. A caller's
            // final hard-link policy must therefore reject publication.
            std::fs::hard_link(path, &alias).map_err(|error| error.to_string())?;
            let links = std::fs::metadata(path)
                .map_err(|error| error.to_string())?
                .nlink();
            if links > 1 {
                Err("destination acquired a hard-link alias".into())
            } else {
                Ok(())
            }
        });
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"original destination"
        );
        assert_eq!(std::fs::read(&alias).unwrap(), b"original destination");
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

    #[cfg(any(
        windows,
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos"
    ))]
    #[test]
    fn failed_generation_publish_retains_its_journal_owned_stage() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        let mut output = AtomicOutput::new_with_overwrite(&destination, false).unwrap();
        output.write_all(b"staged generation").unwrap();
        let stage = output.path().to_owned();
        output.retain_stage();
        std::fs::write(&destination, b"racing destination").unwrap();

        let error = output.generation_publish().unwrap_err();
        assert!(error.contains("without replacing"), "{error}");
        assert_eq!(std::fs::read(&destination).unwrap(), b"racing destination");
        assert_eq!(std::fs::read(&stage).unwrap(), b"staged generation");
    }
}
