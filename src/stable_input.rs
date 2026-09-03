//! Immutable, content-addressed snapshots of local or in-memory inputs.
//!
//! [`StableInput`] keeps every later probe, decode, and render pass on one
//! private snapshot. A path-backed input also retains enough information to
//! detect replacement or in-place modification of the live source without
//! making the path, timestamp, or file identity part of its content binding.

use sha2::{Digest, Sha256};
use std::error::Error;
#[cfg(test)]
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::num::NonZeroU64;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::{Builder, NamedTempFile};

/// Version of the content-binding semantics exposed by this module.
pub const INPUT_CONTENT_BINDING_VERSION: u32 = 1;

const HASH_BUFFER_BYTES: usize = 128 * 1024;

/// Identity of the exact encoded byte stream held by a [`StableInput`].
///
/// The binding deliberately excludes paths, file identities, timestamps, and
/// source-name hints. Equal bindings therefore mean equal byte length and
/// SHA-256 content under binding version 1, even when the bytes came from
/// different locations.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InputContentBinding {
    byte_len: u64,
    sha256: [u8; 32],
}

impl InputContentBinding {
    fn new(byte_len: u64, sha256: [u8; 32]) -> Self {
        Self { byte_len, sha256 }
    }

    /// Content-binding semantic version.
    pub const fn version(&self) -> u32 {
        INPUT_CONTENT_BINDING_VERSION
    }

    /// Number of bytes covered by the digest.
    pub const fn byte_len(&self) -> u64 {
        self.byte_len
    }

    /// SHA-256 digest of the complete encoded byte stream.
    pub const fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }

    /// Lower-case hexadecimal SHA-256 digest.
    pub fn sha256_hex(&self) -> String {
        use std::fmt::Write as _;

        let mut output = String::with_capacity(self.sha256.len() * 2);
        for byte in self.sha256 {
            write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
        }
        output
    }
}

/// Stable classification for failures while capturing or checking an input.
///
/// Error display text is diagnostic and is not a compatibility boundary.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StableInputErrorKind {
    /// The configured encoded-input byte limit is zero.
    InvalidLimit,
    /// A filesystem operation failed.
    Io,
    /// The resolved input is not a regular file.
    NotRegularFile,
    /// The complete byte stream exceeds the caller-provided limit.
    LimitExceeded,
    /// The live source changed while it was captured or after capture.
    SourceChanged,
}

/// Error returned by [`StableInput`] operations.
#[derive(Debug)]
pub struct StableInputError {
    kind: StableInputErrorKind,
    message: String,
    source: Option<io::Error>,
}

impl StableInputError {
    fn new(kind: StableInputErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    fn io(message: impl Into<String>, source: io::Error) -> Self {
        Self {
            kind: StableInputErrorKind::Io,
            message: message.into(),
            source: Some(source),
        }
    }

    fn source_changed(message: impl Into<String>) -> Self {
        Self::new(StableInputErrorKind::SourceChanged, message)
    }

    fn is_not_found(&self) -> bool {
        self.source
            .as_ref()
            .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
    }

    /// Machine-readable error classification.
    pub const fn kind(&self) -> StableInputErrorKind {
        self.kind
    }
}

impl fmt::Display for StableInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)?;
        if let Some(source) = &self.source {
            write!(formatter, ": {source}")?;
        }
        Ok(())
    }
}

impl Error for StableInputError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_ref().map(|source| source as _)
    }
}

/// Resource limit and non-binding source-name hint used during capture.
#[derive(Clone, Debug)]
pub struct StableInputOptions {
    max_input_bytes: NonZeroU64,
    source_name_hint: Option<PathBuf>,
}

impl StableInputOptions {
    /// Create options with an explicit, non-zero encoded-input byte limit.
    pub fn new(max_input_bytes: u64) -> Result<Self, StableInputError> {
        let max_input_bytes = NonZeroU64::new(max_input_bytes).ok_or_else(|| {
            StableInputError::new(
                StableInputErrorKind::InvalidLimit,
                "stable input byte limit must be greater than zero",
            )
        })?;
        Ok(Self {
            max_input_bytes,
            source_name_hint: None,
        })
    }

    /// Attach a non-binding name hint used to preserve a suffix for bytes that
    /// did not originate from a path.
    pub fn with_source_name_hint(mut self, hint: impl Into<PathBuf>) -> Self {
        self.source_name_hint = Some(hint.into());
        self
    }

    /// Maximum accepted encoded-input byte length.
    pub const fn max_input_bytes(&self) -> NonZeroU64 {
        self.max_input_bytes
    }

    /// Optional, non-binding name hint supplied for an in-memory input.
    pub fn source_name_hint(&self) -> Option<&Path> {
        self.source_name_hint.as_deref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum StableFileIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(windows)]
    Windows { volume: u32, index: u64 },
    #[cfg(not(any(unix, windows)))]
    Canonical(PathBuf),
}

#[derive(Debug)]
struct LiveSource {
    path: PathBuf,
    canonical: PathBuf,
    identity: StableFileIdentity,
    byte_len: u64,
    sha256: [u8; 32],
}

/// Path binding retained without copying its payload.
///
/// Metadata workflows can use this to compare aliases and defer the bounded
/// snapshot until they know that another [`StableInput`] cannot be reused.
pub(crate) struct BoundInput {
    file: File,
    source: LiveSource,
    binding: InputContentBinding,
    max_input_bytes: NonZeroU64,
}

impl BoundInput {
    pub(crate) fn bind(
        path: &Path,
        options: &StableInputOptions,
    ) -> Result<Self, StableInputError> {
        let opened = open_source_target(path)?;
        ensure_within_limit(opened.byte_len, options.max_input_bytes.get(), "path input")?;
        let (byte_len, sha256) =
            hash_open_file(&opened.file, options.max_input_bytes.get(), "open source")?;
        if byte_len != opened.byte_len {
            return Err(StableInputError::source_changed(
                "source length changed during its initial hash",
            ));
        }
        let source = LiveSource {
            path: path.to_owned(),
            canonical: opened.canonical,
            identity: opened.identity,
            byte_len,
            sha256,
        };
        verify_live_source(&source, options.max_input_bytes.get())?;
        Ok(Self {
            file: opened.file,
            source,
            binding: InputContentBinding::new(byte_len, sha256),
            max_input_bytes: options.max_input_bytes,
        })
    }

    pub(crate) fn identity(&self) -> &StableFileIdentity {
        &self.source.identity
    }

    pub(crate) fn byte_len(&self) -> u64 {
        self.binding.byte_len()
    }

    pub(crate) fn binding(&self) -> &InputContentBinding {
        &self.binding
    }

    pub(crate) fn verify_source(&self) -> Result<(), StableInputError> {
        verify_live_source(&self.source, self.max_input_bytes.get())
    }

    pub(crate) fn snapshot(self) -> Result<StableInput, StableInputError> {
        let mut snapshot = create_snapshot(Some(&self.source.path))?;
        let (copied, snapshot_sha256) = copy_open_file(
            &self.file,
            snapshot.as_file_mut(),
            self.max_input_bytes.get(),
        )?;
        if copied != self.binding.byte_len() || snapshot_sha256 != *self.binding.sha256() {
            return Err(StableInputError::source_changed(
                "source changed while its private snapshot was captured",
            ));
        }

        let snapshot_identity = identity_from_open_file(snapshot.as_file(), snapshot.path())?;
        verify_live_source(&self.source, self.max_input_bytes.get())?;
        Ok(StableInput {
            inner: Arc::new(StableInputInner {
                snapshot,
                snapshot_identity,
                binding: self.binding,
                max_input_bytes: self.max_input_bytes,
                source_name_hint: Some(self.source.path.clone()),
                live_source: Some(self.source),
            }),
        })
    }
}

struct StableInputInner {
    snapshot: NamedTempFile,
    snapshot_identity: StableFileIdentity,
    binding: InputContentBinding,
    max_input_bytes: NonZeroU64,
    source_name_hint: Option<PathBuf>,
    live_source: Option<LiveSource>,
}

/// An immutable, privately stored input snapshot shared through an [`Arc`].
///
/// Clones share the same snapshot. The private storage path and writable file
/// handle are intentionally not exposed by the public API.
#[derive(Clone)]
pub struct StableInput {
    inner: Arc<StableInputInner>,
}

impl fmt::Debug for StableInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StableInput")
            .field("binding", &self.inner.binding)
            .field("max_input_bytes", &self.inner.max_input_bytes)
            .field("source_name_hint", &self.inner.source_name_hint)
            .field(
                "source_path",
                &self
                    .inner
                    .live_source
                    .as_ref()
                    .map(|source| source.path.as_path()),
            )
            .finish_non_exhaustive()
    }
}

impl StableInput {
    /// Capture a regular file into a bounded private snapshot.
    ///
    /// Bytes are hashed while they are copied from one open source handle.
    /// The source path is then rebound and rehashed so replacement, symlink
    /// retargeting, and in-place modification are rejected before success.
    pub fn from_path(path: &Path, options: &StableInputOptions) -> Result<Self, StableInputError> {
        Self::from_path_impl(path, options, || {})
    }

    /// Copy a byte slice into a bounded private snapshot.
    pub fn from_bytes(
        bytes: &[u8],
        options: &StableInputOptions,
    ) -> Result<Self, StableInputError> {
        let byte_len = u64::try_from(bytes.len()).map_err(|_| {
            StableInputError::new(
                StableInputErrorKind::LimitExceeded,
                "in-memory input length does not fit the supported byte domain",
            )
        })?;
        ensure_within_limit(byte_len, options.max_input_bytes.get(), "in-memory input")?;

        let mut snapshot = create_snapshot(options.source_name_hint())?;
        snapshot
            .as_file_mut()
            .write_all(bytes)
            .map_err(|error| StableInputError::io("write private input snapshot", error))?;
        let (snapshot_len, snapshot_sha256) = hash_open_file(
            snapshot.as_file(),
            options.max_input_bytes.get(),
            "private input snapshot",
        )?;
        if snapshot_len != byte_len {
            return Err(StableInputError::source_changed(
                "private input snapshot length changed while it was written",
            ));
        }
        let snapshot_identity = identity_from_open_file(snapshot.as_file(), snapshot.path())?;

        Ok(Self {
            inner: Arc::new(StableInputInner {
                snapshot,
                snapshot_identity,
                binding: InputContentBinding::new(snapshot_len, snapshot_sha256),
                max_input_bytes: options.max_input_bytes,
                source_name_hint: options.source_name_hint.clone(),
                live_source: None,
            }),
        })
    }

    fn from_path_impl(
        path: &Path,
        options: &StableInputOptions,
        after_initial_hash: impl FnOnce(),
    ) -> Result<Self, StableInputError> {
        let opened = open_source_target(path)?;
        ensure_within_limit(opened.byte_len, options.max_input_bytes.get(), "path input")?;
        let mut snapshot = create_snapshot(Some(path))?;
        let (byte_len, sha256) = copy_open_file(
            &opened.file,
            snapshot.as_file_mut(),
            options.max_input_bytes.get(),
        )?;
        if byte_len != opened.byte_len {
            return Err(StableInputError::source_changed(
                "source length changed while its private snapshot was captured",
            ));
        }
        let source = LiveSource {
            path: path.to_owned(),
            canonical: opened.canonical,
            identity: opened.identity,
            byte_len,
            sha256,
        };
        after_initial_hash();
        verify_live_source(&source, options.max_input_bytes.get())?;
        let snapshot_identity = identity_from_open_file(snapshot.as_file(), snapshot.path())?;
        Ok(Self {
            inner: Arc::new(StableInputInner {
                snapshot,
                snapshot_identity,
                binding: InputContentBinding::new(byte_len, sha256),
                max_input_bytes: options.max_input_bytes,
                source_name_hint: Some(path.to_owned()),
                live_source: Some(source),
            }),
        })
    }

    #[cfg(test)]
    fn from_path_with_hook(
        path: &Path,
        options: &StableInputOptions,
        after_initial_hash: impl FnOnce(),
    ) -> Result<Self, StableInputError> {
        Self::from_path_impl(path, options, after_initial_hash)
    }

    /// Identity of the complete byte stream held in the private snapshot.
    pub fn binding(&self) -> &InputContentBinding {
        &self.inner.binding
    }

    /// Original path for path-backed inputs, or `None` for in-memory inputs.
    ///
    /// This path is informational and is not used for later decoding.
    pub fn source_path(&self) -> Option<&Path> {
        self.inner
            .live_source
            .as_ref()
            .map(|source| source.path.as_path())
    }

    /// Non-binding name used to preserve the source suffix.
    pub fn source_name_hint(&self) -> Option<&Path> {
        self.inner.source_name_hint.as_deref()
    }

    /// Encoded-input byte limit used to create and verify this snapshot.
    pub fn max_input_bytes(&self) -> NonZeroU64 {
        self.inner.max_input_bytes
    }

    /// Number of bytes retained by this snapshot.
    pub fn byte_len(&self) -> u64 {
        self.inner.binding.byte_len()
    }

    /// SHA-256 digest of the complete retained byte stream.
    pub fn sha256(&self) -> &[u8; 32] {
        self.inner.binding.sha256()
    }

    /// Verify that a path-backed live source still names the captured file with
    /// the captured length and content. In-memory inputs always verify.
    pub fn verify_source(&self) -> Result<(), StableInputError> {
        match &self.inner.live_source {
            Some(source) => verify_live_source(source, self.inner.max_input_bytes.get()),
            None => Ok(()),
        }
    }

    /// Private path used by decoder and rendering integrations.
    #[allow(dead_code)]
    pub(crate) fn stable_path(&self) -> &Path {
        self.inner.snapshot.path()
    }

    /// Whether an existing path resolves to the path-backed live source.
    #[allow(dead_code)]
    pub(crate) fn aliases_source_path(&self, path: &Path) -> Result<bool, StableInputError> {
        let Some(source) = &self.inner.live_source else {
            return Ok(false);
        };
        Ok(path_identity_if_exists(path)?.is_some_and(|identity| identity == source.identity))
    }

    /// Whether an existing path resolves to the private snapshot itself.
    #[allow(dead_code)]
    pub(crate) fn aliases_snapshot_path(&self, path: &Path) -> Result<bool, StableInputError> {
        Ok(path_identity_if_exists(path)?
            .is_some_and(|identity| identity == self.inner.snapshot_identity))
    }

    /// Identity of the live source handle captured for a path-backed input.
    #[allow(dead_code)]
    pub(crate) fn source_identity(&self) -> Option<&StableFileIdentity> {
        self.inner
            .live_source
            .as_ref()
            .map(|source| &source.identity)
    }

    /// Identity of the private snapshot file.
    #[allow(dead_code)]
    pub(crate) fn snapshot_identity(&self) -> &StableFileIdentity {
        &self.inner.snapshot_identity
    }

    /// Whether an already-open file aliases either the captured source or the
    /// private snapshot. Callers can use this before writing an output handle.
    #[allow(dead_code)]
    pub(crate) fn aliases_open_file(
        &self,
        file: &File,
        path: &Path,
    ) -> Result<bool, StableInputError> {
        let identity = identity_from_open_file(file, path)?;
        Ok(identity == self.inner.snapshot_identity
            || self
                .inner
                .live_source
                .as_ref()
                .is_some_and(|source| identity == source.identity))
    }
}

/// Compare two existing paths by the identity of the file each path opens.
/// Missing paths do not alias; other open or inspection failures are reported.
pub(crate) fn paths_alias_if_existing(left: &Path, right: &Path) -> Result<bool, StableInputError> {
    let Some(left) = path_identity_if_exists(left)? else {
        return Ok(false);
    };
    Ok(path_identity_if_exists(right)?.is_some_and(|right| left == right))
}

struct OpenedSource {
    file: File,
    canonical: PathBuf,
    identity: StableFileIdentity,
    byte_len: u64,
}

fn open_source_target(path: &Path) -> Result<OpenedSource, StableInputError> {
    let canonical = fs::canonicalize(path).map_err(|error| {
        StableInputError::io(format!("resolve input source {}", path.display()), error)
    })?;
    let file = open_final_target(&canonical)?;
    let canonical_after = fs::canonicalize(path).map_err(|error| {
        StableInputError::io(format!("re-resolve input source {}", path.display()), error)
    })?;
    if canonical_after != canonical {
        return Err(StableInputError::source_changed(format!(
            "input source changed while it was opened: {}",
            path.display()
        )));
    }
    let metadata = file.metadata().map_err(|error| {
        StableInputError::io(format!("inspect input source {}", path.display()), error)
    })?;
    #[cfg(windows)]
    if metadata.file_attributes() & 0x0000_0400 != 0 {
        return Err(StableInputError::new(
            StableInputErrorKind::NotRegularFile,
            format!("input source is a reparse point: {}", path.display()),
        ));
    }
    if !metadata.is_file() {
        return Err(StableInputError::new(
            StableInputErrorKind::NotRegularFile,
            format!("input source is not a regular file: {}", path.display()),
        ));
    }
    let identity = file_identity(&file, &metadata, &canonical)?;
    Ok(OpenedSource {
        file,
        canonical,
        identity,
        byte_len: metadata.len(),
    })
}

fn open_final_target(path: &Path) -> Result<File, StableInputError> {
    let mut options = OpenOptions::new();
    options.read(true);
    // The caller resolves the user-facing path first. Refusing to follow the
    // final component closes the exchange window between resolution and open.
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    // Open a Windows reparse point itself; open_source_target then rejects it.
    #[cfg(windows)]
    options.custom_flags(0x0020_0000);
    options.open(path).map_err(|error| {
        StableInputError::io(format!("open final input target {}", path.display()), error)
    })
}

fn verify_live_source(source: &LiveSource, max_bytes: u64) -> Result<(), StableInputError> {
    let current = open_source_target(&source.path).map_err(|error| {
        StableInputError::source_changed(format!(
            "live input source can no longer be rebound: {}: {error}",
            source.path.display()
        ))
    })?;
    if current.canonical != source.canonical
        || current.identity != source.identity
        || current.byte_len != source.byte_len
    {
        return Err(StableInputError::source_changed(format!(
            "live input source identity or length changed: {}",
            source.path.display()
        )));
    }
    let (byte_len, sha256) = hash_open_file(&current.file, max_bytes, "live input source")
        .map_err(|error| {
            StableInputError::source_changed(format!(
                "live input source could not be verified: {}: {error}",
                source.path.display()
            ))
        })?;
    if byte_len != source.byte_len || sha256 != source.sha256 {
        return Err(StableInputError::source_changed(format!(
            "live input source content changed: {}",
            source.path.display()
        )));
    }
    Ok(())
}

fn ensure_within_limit(
    byte_len: u64,
    max_bytes: u64,
    description: &str,
) -> Result<(), StableInputError> {
    if byte_len > max_bytes {
        return Err(StableInputError::new(
            StableInputErrorKind::LimitExceeded,
            format!(
                "{description} is {byte_len} bytes, above the configured byte limit {max_bytes}"
            ),
        ));
    }
    Ok(())
}

fn create_snapshot(source_name_hint: Option<&Path>) -> Result<NamedTempFile, StableInputError> {
    // The snapshot is process-local scratch state, not a restartable artifact.
    // Completed writes are immediately visible to its open file descriptor;
    // durability flushes belong only to final output publication.
    let mut builder = Builder::new();
    builder.prefix("forge-stable-input-");
    let suffix = source_name_hint.and_then(snapshot_suffix);
    if let Some(suffix) = &suffix {
        builder.suffix(suffix);
    }
    builder
        .tempfile()
        .map_err(|error| StableInputError::io("create private input snapshot", error))
}

fn snapshot_suffix(path: &Path) -> Option<OsString> {
    let extension = path.extension()?;
    let mut suffix = OsString::from(".");
    suffix.push(extension);
    Some(suffix)
}

fn copy_open_file(
    source: &File,
    destination: &mut File,
    max_bytes: u64,
) -> Result<(u64, [u8; 32]), StableInputError> {
    let mut source = source
        .try_clone()
        .map_err(|error| StableInputError::io("clone open source for snapshot", error))?;
    source
        .seek(SeekFrom::Start(0))
        .map_err(|error| StableInputError::io("seek open source for snapshot", error))?;
    let mut total = 0_u64;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; HASH_BUFFER_BYTES];
    loop {
        let read = source
            .read(&mut buffer)
            .map_err(|error| StableInputError::io("read open source for snapshot", error))?;
        if read == 0 {
            break;
        }
        total = total.checked_add(read as u64).ok_or_else(|| {
            StableInputError::new(
                StableInputErrorKind::LimitExceeded,
                "input byte length overflow while creating snapshot",
            )
        })?;
        ensure_within_limit(total, max_bytes, "input source")?;
        hasher.update(&buffer[..read]);
        destination
            .write_all(&buffer[..read])
            .map_err(|error| StableInputError::io("write private input snapshot", error))?;
    }
    Ok((total, hasher.finalize().into()))
}

fn hash_open_file(
    file: &File,
    max_bytes: u64,
    description: &str,
) -> Result<(u64, [u8; 32]), StableInputError> {
    let mut input = file
        .try_clone()
        .map_err(|error| StableInputError::io(format!("clone {description}"), error))?;
    input
        .seek(SeekFrom::Start(0))
        .map_err(|error| StableInputError::io(format!("seek {description}"), error))?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; HASH_BUFFER_BYTES];
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|error| StableInputError::io(format!("read {description}"), error))?;
        if read == 0 {
            break;
        }
        total = total.checked_add(read as u64).ok_or_else(|| {
            StableInputError::new(
                StableInputErrorKind::LimitExceeded,
                format!("{description} byte length overflow"),
            )
        })?;
        ensure_within_limit(total, max_bytes, description)?;
        hasher.update(&buffer[..read]);
    }
    Ok((total, hasher.finalize().into()))
}

pub(crate) fn identity_from_open_file(
    file: &File,
    path: &Path,
) -> Result<StableFileIdentity, StableInputError> {
    let metadata = file.metadata().map_err(|error| {
        StableInputError::io(format!("inspect opened file {}", path.display()), error)
    })?;
    let canonical = fs::canonicalize(path).map_err(|error| {
        StableInputError::io(format!("resolve opened file {}", path.display()), error)
    })?;
    file_identity(file, &metadata, &canonical)
}

fn file_identity(
    file: &File,
    metadata: &fs::Metadata,
    canonical: &Path,
) -> Result<StableFileIdentity, StableInputError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = (file, canonical);
        Ok(StableFileIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(windows)]
    {
        let _ = (metadata, canonical);
        let (volume, index) = windows_file_identity(file)?;
        Ok(StableFileIdentity::Windows { volume, index })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, metadata);
        Ok(StableFileIdentity::Canonical(canonical.to_owned()))
    }
}

fn path_identity_if_exists(path: &Path) -> Result<Option<StableFileIdentity>, StableInputError> {
    let opened = match open_source_target(path) {
        Ok(opened) => opened,
        Err(error) if error.is_not_found() => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(Some(opened.identity))
}

#[cfg(windows)]
fn windows_file_identity(file: &File) -> Result<(u32, u64), StableInputError> {
    let information = windows_file_information(file)?;
    Ok((
        information.dwVolumeSerialNumber,
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    ))
}

#[cfg(windows)]
#[repr(C)]
#[allow(non_snake_case)]
struct WindowsFileTime {
    dwLowDateTime: u32,
    dwHighDateTime: u32,
}

#[cfg(windows)]
#[repr(C)]
#[allow(non_snake_case)]
struct WindowsByHandleFileInformation {
    dwFileAttributes: u32,
    ftCreationTime: WindowsFileTime,
    ftLastAccessTime: WindowsFileTime,
    ftLastWriteTime: WindowsFileTime,
    dwVolumeSerialNumber: u32,
    nFileSizeHigh: u32,
    nFileSizeLow: u32,
    nNumberOfLinks: u32,
    nFileIndexHigh: u32,
    nFileIndexLow: u32,
}

#[cfg(windows)]
fn windows_file_information(
    file: &File,
) -> Result<WindowsByHandleFileInformation, StableInputError> {
    use std::ffi::c_void;
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileInformationByHandle(
            handle: *mut c_void,
            information: *mut WindowsByHandleFileInformation,
        ) -> i32;
    }

    let mut information = MaybeUninit::<WindowsByHandleFileInformation>::uninit();
    let succeeded = unsafe {
        GetFileInformationByHandle(file.as_raw_handle().cast(), information.as_mut_ptr())
    };
    if succeeded == 0 {
        return Err(StableInputError::io(
            "identify open Windows file",
            io::Error::last_os_error(),
        ));
    }
    Ok(unsafe { information.assume_init() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(limit: u64) -> StableInputOptions {
        StableInputOptions::new(limit).unwrap()
    }

    #[test]
    fn zero_limit_is_rejected() {
        assert_eq!(
            StableInputOptions::new(0).unwrap_err().kind(),
            StableInputErrorKind::InvalidLimit
        );
    }

    #[test]
    fn identical_bytes_have_identical_content_bindings() {
        let first = StableInput::from_bytes(b"abc", &options(16)).unwrap();
        let second = StableInput::from_bytes(b"abc", &options(16)).unwrap();

        assert_eq!(first.binding(), second.binding());
        assert_eq!(first.binding().version(), INPUT_CONTENT_BINDING_VERSION);
        assert_eq!(first.binding().byte_len(), 3);
        assert_eq!(
            first.binding().sha256_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn one_byte_difference_changes_the_content_binding() {
        let first = StableInput::from_bytes(b"abc", &options(16)).unwrap();
        let second = StableInput::from_bytes(b"abd", &options(16)).unwrap();

        assert_ne!(first.binding(), second.binding());
    }

    #[test]
    fn snapshot_survives_source_rename_and_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.wav");
        let renamed = directory.path().join("renamed.wav");
        fs::write(&source, b"stable bytes").unwrap();
        let input = StableInput::from_path(&source, &options(64)).unwrap();

        fs::rename(&source, &renamed).unwrap();
        assert_eq!(fs::read(input.stable_path()).unwrap(), b"stable bytes");
        fs::remove_file(&renamed).unwrap();
        assert_eq!(fs::read(input.stable_path()).unwrap(), b"stable bytes");
        assert_eq!(
            input.verify_source().unwrap_err().kind(),
            StableInputErrorKind::SourceChanged
        );
    }

    #[test]
    fn live_verification_detects_same_length_in_place_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        fs::write(&source, b"AAAAAA").unwrap();
        let input = StableInput::from_path(&source, &options(64)).unwrap();

        fs::write(&source, b"BBBBBB").unwrap();
        assert_eq!(
            input.verify_source().unwrap_err().kind(),
            StableInputErrorKind::SourceChanged
        );
        assert_eq!(fs::read(input.stable_path()).unwrap(), b"AAAAAA");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn capture_detects_same_length_hardlink_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        let alias = directory.path().join("alias.bin");
        fs::write(&source, b"AAAAAA").unwrap();
        fs::hard_link(&source, &alias).unwrap();

        let error = StableInput::from_path_with_hook(&source, &options(64), || {
            fs::write(&alias, b"BBBBBB").unwrap();
        })
        .unwrap_err();
        assert_eq!(error.kind(), StableInputErrorKind::SourceChanged);
    }

    #[test]
    fn capture_detects_rename_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        let displaced = directory.path().join("displaced.bin");
        let replacement = directory.path().join("replacement.bin");
        fs::write(&source, b"AAAAAA").unwrap();
        fs::write(&replacement, b"BBBBBB").unwrap();

        let error = StableInput::from_path_with_hook(&source, &options(64), || {
            fs::rename(&source, &displaced).unwrap();
            fs::rename(&replacement, &source).unwrap();
        })
        .unwrap_err();
        assert_eq!(error.kind(), StableInputErrorKind::SourceChanged);
    }

    #[cfg(unix)]
    #[test]
    fn capture_detects_symlink_retargeting() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.wav");
        let original = directory.path().join("original.wav");
        let replacement = directory.path().join("replacement.wav");
        fs::write(&original, b"AAAAAA").unwrap();
        fs::write(&replacement, b"BBBBBB").unwrap();
        symlink(&original, &source).unwrap();

        let error = StableInput::from_path_with_hook(&source, &options(64), || {
            fs::remove_file(&source).unwrap();
            symlink(&replacement, &source).unwrap();
        })
        .unwrap_err();
        assert_eq!(error.kind(), StableInputErrorKind::SourceChanged);
    }

    #[cfg(unix)]
    #[test]
    fn final_target_open_does_not_follow_a_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.bin");
        let link = directory.path().join("link.bin");
        fs::write(&target, b"target").unwrap();
        symlink(&target, &link).unwrap();

        assert!(open_final_target(&link).is_err());
    }

    #[test]
    fn limits_and_regular_file_requirement_are_enforced() {
        let memory_error = StableInput::from_bytes(b"four", &options(3)).unwrap_err();
        assert_eq!(memory_error.kind(), StableInputErrorKind::LimitExceeded);

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        fs::write(&source, b"four").unwrap();
        let path_error = StableInput::from_path(&source, &options(3)).unwrap_err();
        assert_eq!(path_error.kind(), StableInputErrorKind::LimitExceeded);

        let regular_error = StableInput::from_path(directory.path(), &options(64)).unwrap_err();
        assert_eq!(regular_error.kind(), StableInputErrorKind::NotRegularFile);
    }

    #[test]
    fn source_suffix_is_preserved_without_leaking_snapshot_path_in_debug() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.WAV");
        fs::write(&source, b"bytes").unwrap();
        let path_input = StableInput::from_path(&source, &options(64)).unwrap();
        assert_eq!(
            path_input.stable_path().extension(),
            Some(OsStr::new("WAV"))
        );

        let byte_options = options(64).with_source_name_hint("memory.flac");
        let byte_input = StableInput::from_bytes(b"bytes", &byte_options).unwrap();
        assert_eq!(
            byte_input.stable_path().extension(),
            Some(OsStr::new("flac"))
        );
        assert!(!format!("{byte_input:?}")
            .contains(&byte_input.stable_path().to_string_lossy().into_owned()));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn alias_helpers_compare_open_file_identity() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        let alias = directory.path().join("alias.bin");
        let unrelated = directory.path().join("unrelated.bin");
        fs::write(&source, b"source").unwrap();
        fs::hard_link(&source, &alias).unwrap();
        fs::write(&unrelated, b"source").unwrap();
        let input = StableInput::from_path(&source, &options(64)).unwrap();

        assert!(input.aliases_source_path(&alias).unwrap());
        assert!(!input.aliases_source_path(&unrelated).unwrap());
        assert!(input.aliases_snapshot_path(input.stable_path()).unwrap());
    }

    #[test]
    fn clones_share_one_snapshot() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<StableInput>();

        let input = StableInput::from_bytes(b"shared", &options(64)).unwrap();
        let cloned = input.clone();
        assert_eq!(input.stable_path(), cloned.stable_path());
    }
}
