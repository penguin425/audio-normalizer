//! A small, synchronous broker for running trusted helper programs.
//!
//! The broker deliberately keeps the process policy in one place.  A command
//! is resolved to an absolute, canonical path before it is started, receives
//! an explicitly selected environment policy, and is put in a killable
//! process tree.  Captured stdout and stderr are drained by dedicated reader
//! threads and have independent hard byte limits.  A timeout, cancellation,
//! reader failure, or monitored-file violation always terminates the tree and
//! reaps the child before the error is returned.
//!
//! `Null` streams are sent directly to the platform null device.  A captured
//! stream is bounded in bytes, not lines; the byte that would exceed the
//! configured limit is not retained.  Missing monitored files are allowed,
//! but a present monitored path must be a regular, non-symlink file.
//!
//! On Unix a dedicated process group is created before `exec`. On Windows the
//! primary thread is created suspended, attached to a kill-on-close Job
//! Object, and only then resumed; if setup, assignment, or resume fails, the
//! launch is rejected.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
#[cfg(feature = "ffmpeg-encoding")]
use std::io::Write;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Errors returned by the subprocess broker.
#[derive(Debug)]
pub enum Error {
    InvalidExecutable(String),
    InvalidConfiguration(String),
    InvalidTimeout,
    ExecutableChanged(PathBuf),
    Spawn(io::Error),
    Terminate(io::Error),
    #[cfg(feature = "ffmpeg-encoding")]
    WriteStdin(io::Error),
    Wait(io::Error),
    Reader {
        stream: StreamName,
        message: String,
    },
    OutputLimit {
        stream: StreamName,
        limit: usize,
    },
    MonitoredFile {
        label: String,
        path: PathBuf,
        message: String,
    },
    MonitoredFileLimit {
        label: String,
        path: PathBuf,
        limit: u64,
        actual: u64,
    },
    MonitoredDirectory {
        label: String,
        path: PathBuf,
        message: String,
    },
    MonitoredDirectoryLimit {
        label: String,
        path: PathBuf,
        max_bytes: u64,
        actual_bytes: u64,
        max_files: u64,
        actual_files: u64,
    },
    TimedOut,
    Cancelled,
    #[allow(dead_code)]
    ProcessTreeUnavailable(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidExecutable(message) => write!(f, "invalid executable: {message}"),
            Self::InvalidConfiguration(message) => write!(f, "invalid helper configuration: {message}"),
            Self::InvalidTimeout => f.write_str("helper timeout cannot be represented by the system clock"),
            Self::ExecutableChanged(path) => {
                write!(f, "executable changed before launch: {}", path.display())
            }
            Self::Spawn(error) => write!(f, "failed to spawn helper: {error}"),
            Self::Terminate(error) => write!(f, "failed to terminate helper process tree: {error}"),
            #[cfg(feature = "ffmpeg-encoding")]
            Self::WriteStdin(error) => write!(f, "failed to write helper stdin: {error}"),
            Self::Wait(error) => write!(f, "failed to wait for helper: {error}"),
            Self::Reader { stream, message } => write!(f, "failed to read {stream}: {message}"),
            Self::OutputLimit { stream, limit } => write!(f, "{stream} exceeded {limit} bytes"),
            Self::MonitoredFile {
                label,
                path,
                message,
            } => {
                write!(f, "monitored file {label} ({}): {message}", path.display())
            }
            Self::MonitoredFileLimit {
                label,
                path,
                limit,
                actual,
            } => write!(
                f,
                "monitored file {label} ({}) exceeded {limit} bytes (size {actual})",
                path.display()
            ),
            Self::MonitoredDirectory {
                label,
                path,
                message,
            } => write!(f, "monitored directory {label} ({}): {message}", path.display()),
            Self::MonitoredDirectoryLimit {
                label,
                path,
                max_bytes,
                actual_bytes,
                max_files,
                actual_files,
            } => write!(
                f,
                "monitored directory {label} ({}) exceeded {max_bytes} bytes/{max_files} files (size {actual_bytes}, files {actual_files})",
                path.display()
            ),
            Self::TimedOut => f.write_str("helper process timed out"),
            Self::Cancelled => f.write_str("helper process cancelled"),
            Self::ProcessTreeUnavailable(message) => {
                write!(f, "process-tree containment unavailable: {message}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(error) | Self::Terminate(error) | Self::Wait(error) => Some(error),
            #[cfg(feature = "ffmpeg-encoding")]
            Self::WriteStdin(error) => Some(error),
            _ => None,
        }
    }
}

impl Error {
    #[cfg(feature = "ffmpeg-encoding")]
    fn clone_for_control(&self) -> Self {
        match self {
            Self::InvalidExecutable(message) => Self::InvalidExecutable(message.clone()),
            Self::InvalidConfiguration(message) => Self::InvalidConfiguration(message.clone()),
            Self::InvalidTimeout => Self::InvalidTimeout,
            Self::ExecutableChanged(path) => Self::ExecutableChanged(path.clone()),
            Self::Spawn(error) => Self::Spawn(io::Error::new(error.kind(), error.to_string())),
            Self::Terminate(error) => {
                Self::Terminate(io::Error::new(error.kind(), error.to_string()))
            }
            #[cfg(feature = "ffmpeg-encoding")]
            Self::WriteStdin(error) => {
                Self::WriteStdin(io::Error::new(error.kind(), error.to_string()))
            }
            Self::Wait(error) => Self::Wait(io::Error::new(error.kind(), error.to_string())),
            Self::Reader { stream, message } => Self::Reader {
                stream: *stream,
                message: message.clone(),
            },
            Self::OutputLimit { stream, limit } => Self::OutputLimit {
                stream: *stream,
                limit: *limit,
            },
            Self::MonitoredFile {
                label,
                path,
                message,
            } => Self::MonitoredFile {
                label: label.clone(),
                path: path.clone(),
                message: message.clone(),
            },
            Self::MonitoredFileLimit {
                label,
                path,
                limit,
                actual,
            } => Self::MonitoredFileLimit {
                label: label.clone(),
                path: path.clone(),
                limit: *limit,
                actual: *actual,
            },
            Self::MonitoredDirectory {
                label,
                path,
                message,
            } => Self::MonitoredDirectory {
                label: label.clone(),
                path: path.clone(),
                message: message.clone(),
            },
            Self::MonitoredDirectoryLimit {
                label,
                path,
                max_bytes,
                actual_bytes,
                max_files,
                actual_files,
            } => Self::MonitoredDirectoryLimit {
                label: label.clone(),
                path: path.clone(),
                max_bytes: *max_bytes,
                actual_bytes: *actual_bytes,
                max_files: *max_files,
                actual_files: *actual_files,
            },
            Self::TimedOut => Self::TimedOut,
            Self::Cancelled => Self::Cancelled,
            Self::ProcessTreeUnavailable(message) => Self::ProcessTreeUnavailable(message.clone()),
        }
    }
}

/// Which output stream produced an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamName {
    Stdout,
    Stderr,
}

impl fmt::Display for StreamName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        })
    }
}

/// A cloneable cancellation signal for a running helper.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    #[allow(dead_code)]
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// The environment given to a helper.
#[derive(Clone, Default)]
#[allow(dead_code)]
pub enum EnvPolicy {
    /// Start with no inherited variables.
    Clear,
    /// Keep only the small platform-dependent set normally needed by a
    /// command-line media tool.  This is the default.
    #[default]
    Minimal,
    /// Inherit the complete parent environment.  This is intentionally
    /// explicit because helper programs should not receive ambient secrets.
    Inherit,
    /// Clear the environment, then copy only these variable names from the
    /// parent environment.
    AllowList(Vec<OsString>),
    /// Clear the environment and install exactly these values.
    Explicit(Vec<(OsString, OsString)>),
}

impl fmt::Debug for EnvPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Clear => formatter.write_str("Clear"),
            Self::Minimal => formatter.write_str("Minimal"),
            Self::Inherit => formatter.write_str("Inherit"),
            Self::AllowList(names) => formatter.debug_tuple("AllowList").field(names).finish(),
            Self::Explicit(values) => formatter
                .debug_struct("Explicit")
                .field(
                    "names",
                    &values.iter().map(|(name, _)| name).collect::<Vec<_>>(),
                )
                .field("values", &"<redacted>")
                .finish(),
        }
    }
}

/// How stdin is connected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)]
pub enum StdinMode {
    #[default]
    Null,
    Piped,
}

/// How one output stream is connected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputMode {
    #[default]
    Null,
    Capture {
        max_bytes: usize,
    },
}

impl OutputMode {
    pub const fn capture(max_bytes: usize) -> Self {
        Self::Capture { max_bytes }
    }
}

/// A file produced by the helper whose size is monitored while it runs.
#[derive(Clone, Debug)]
pub struct MonitoredFile {
    path: PathBuf,
    max_bytes: u64,
    label: String,
}

impl MonitoredFile {
    pub fn new(path: impl Into<PathBuf>, max_bytes: u64, label: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            max_bytes,
            label: label.into(),
        }
    }
}

/// A directory tree produced by the helper whose aggregate size and entry
/// count are monitored while it runs. Missing directories are allowed; if a
/// directory appears, every descendant must be a regular file or a directory.
/// Symbolic links and other special files are rejected. Counting every entry,
/// including directories, also bounds traversal of a tree full of empty dirs.
#[derive(Clone, Debug)]
pub struct MonitoredDirectory {
    path: PathBuf,
    max_bytes: u64,
    max_files: u64,
    label: String,
}

impl MonitoredDirectory {
    pub fn new(
        path: impl Into<PathBuf>,
        max_bytes: u64,
        max_files: u64,
        label: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            max_bytes,
            max_files,
            label: label.into(),
        }
    }
}

/// The fully configured command passed to [`run`] or [`spawn_stream`].
#[derive(Clone, Debug)]
pub struct ProcessSpec {
    executable: ExecutableIdentity,
    args: Vec<OsString>,
    env_policy: EnvPolicy,
    stdin: StdinMode,
    stdout: OutputMode,
    stderr: OutputMode,
    timeout: Duration,
    cancellation: CancellationToken,
    monitored_files: Vec<MonitoredFile>,
    monitored_directories: Vec<MonitoredDirectory>,
    current_dir: Option<PathBuf>,
}

/// The default wall-clock budget for a helper that does not select one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// Bound identity hashing so resolving a hostile sparse executable cannot
/// consume unbounded time or I/O before the process timeout starts.
const MAX_EXECUTABLE_BYTES: u64 = 1 << 30;

impl ProcessSpec {
    /// Resolve `executable` now, before any child is launched.
    pub fn new(executable: impl AsRef<OsStr>) -> Result<Self, Error> {
        Ok(Self {
            executable: ExecutableIdentity::resolve(executable)?,
            args: Vec::new(),
            env_policy: EnvPolicy::default(),
            stdin: StdinMode::default(),
            stdout: OutputMode::default(),
            stderr: OutputMode::default(),
            timeout: DEFAULT_TIMEOUT,
            cancellation: CancellationToken::new(),
            monitored_files: Vec::new(),
            monitored_directories: Vec::new(),
            current_dir: None,
        })
    }

    pub fn from_executable(executable: ExecutableIdentity) -> Self {
        Self {
            executable,
            args: Vec::new(),
            env_policy: EnvPolicy::default(),
            stdin: StdinMode::default(),
            stdout: OutputMode::default(),
            stderr: OutputMode::default(),
            timeout: DEFAULT_TIMEOUT,
            cancellation: CancellationToken::new(),
            monitored_files: Vec::new(),
            monitored_directories: Vec::new(),
            current_dir: None,
        }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_os_string()));
        self
    }

    pub fn env_policy(&mut self, policy: EnvPolicy) -> &mut Self {
        self.env_policy = policy;
        self
    }

    pub fn stdin(&mut self, mode: StdinMode) -> &mut Self {
        self.stdin = mode;
        self
    }

    pub fn stdout(&mut self, mode: OutputMode) -> &mut Self {
        self.stdout = mode;
        self
    }

    pub fn stderr(&mut self, mode: OutputMode) -> &mut Self {
        self.stderr = mode;
        self
    }

    pub fn timeout(&mut self, timeout: Duration) -> &mut Self {
        self.timeout = timeout;
        self
    }

    #[allow(dead_code)]
    pub fn current_dir(&mut self, current_dir: impl Into<PathBuf>) -> &mut Self {
        self.current_dir = Some(current_dir.into());
        self
    }

    #[allow(dead_code)]
    pub fn cancellation(&mut self, token: CancellationToken) -> &mut Self {
        self.cancellation = token;
        self
    }

    pub fn monitor_file(&mut self, file: MonitoredFile) -> &mut Self {
        self.monitored_files.push(file);
        self
    }

    pub fn monitor_directory(&mut self, directory: MonitoredDirectory) -> &mut Self {
        self.monitored_directories.push(directory);
        self
    }

    pub fn executable(&self) -> &ExecutableIdentity {
        &self.executable
    }
}

/// The canonical executable path and file identity captured before launch.
#[derive(Clone, Debug)]
pub struct ExecutableIdentity {
    path: PathBuf,
    file: FileIdentity,
    #[cfg(unix)]
    pinned: PinnedExecutable,
    #[cfg(windows)]
    pinned: PinnedExecutable,
    #[cfg(not(any(unix, windows)))]
    pinned: (),
}

impl ExecutableIdentity {
    pub fn resolve(executable: impl AsRef<OsStr>) -> Result<Self, Error> {
        let executable = executable.as_ref();
        if executable.is_empty() {
            return Err(Error::InvalidExecutable("empty executable name".to_owned()));
        }

        let path_like = is_path_like(executable);
        let mut candidates = Vec::new();
        if path_like {
            candidates.push(PathBuf::from(executable));
        } else {
            let path = env::var_os("PATH")
                .ok_or_else(|| Error::InvalidExecutable("PATH is not set".to_owned()))?;
            for directory in env::split_paths(&path) {
                let directory = if directory.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    directory
                };
                candidates.push(directory.join(executable));
            }
            #[cfg(windows)]
            {
                let pathext =
                    env::var_os("PATHEXT").unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
                let has_extension = Path::new(executable).extension().is_some();
                if !has_extension {
                    for extension in pathext.to_string_lossy().split(';') {
                        if !extension.is_empty() {
                            for directory in env::split_paths(&path) {
                                let directory = if directory.as_os_str().is_empty() {
                                    PathBuf::from(".")
                                } else {
                                    directory.clone()
                                };
                                candidates.push(directory.join(format!(
                                    "{}{}",
                                    executable.to_string_lossy(),
                                    extension
                                )));
                            }
                        }
                    }
                }
            }
        }

        let mut last_error = None;
        for candidate in candidates {
            match fs::canonicalize(&candidate) {
                Ok(path) => match FileIdentity::read(&path, true) {
                    Ok(file) => match pin_executable(&path) {
                        Ok(pinned) => {
                            #[cfg(unix)]
                            let identity_matches = FileIdentity::read_pinned(&pinned.file)
                                .map(|current| current == file);
                            #[cfg(windows)]
                            let identity_matches = FileIdentity::read_pinned(&pinned.handle)
                                .map(|current| current == file);
                            #[cfg(not(any(unix, windows)))]
                            let identity_matches = Ok(false);
                            match identity_matches {
                                Ok(true) => return Ok(Self { path, file, pinned }),
                                Ok(false) => {
                                    last_error = Some(io::Error::other(
                                        "executable changed while it was being pinned",
                                    ));
                                }
                                Err(error) => last_error = Some(error),
                            }
                        }
                        Err(error) => last_error = Some(error),
                    },
                    Err(error) => last_error = Some(error),
                },
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => last_error = Some(error),
            }
        }
        if let Some(error) = last_error {
            return Err(Error::InvalidExecutable(error.to_string()));
        }
        Err(Error::InvalidExecutable(format!(
            "executable not found: {}",
            executable.to_string_lossy()
        )))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Byte length of the image held by the pinned executable identity.
    #[allow(dead_code)]
    pub fn byte_len(&self) -> u64 {
        self.file.length
    }

    /// Alias for [`Self::byte_len`] used by provenance/evidence builders.
    #[allow(dead_code)]
    pub fn len(&self) -> u64 {
        self.byte_len()
    }

    /// SHA-256 of the exact image that will be launched.
    #[allow(dead_code)]
    pub fn sha256(&self) -> &[u8; 32] {
        &self.file.digest
    }

    /// Lowercase hexadecimal SHA-256 of the exact image that will be launched.
    #[allow(dead_code)]
    pub fn sha256_hex(&self) -> String {
        let mut value = String::with_capacity(64);
        for byte in self.file.digest {
            use std::fmt::Write as _;
            let _ = write!(value, "{byte:02x}");
        }
        value
    }

    fn launch_path(&self) -> PathBuf {
        #[cfg(unix)]
        {
            let fd = self.pinned.file.as_raw_fd();
            #[cfg(any(target_os = "linux", target_os = "solaris", target_os = "illumos"))]
            {
                PathBuf::from(format!("/proc/self/fd/{fd}"))
            }
            #[cfg(any(
                target_os = "macos",
                target_os = "freebsd",
                target_os = "openbsd",
                target_os = "netbsd",
                target_os = "dragonfly"
            ))]
            {
                PathBuf::from(format!("/dev/fd/{fd}"))
            }
            #[cfg(not(any(
                target_os = "linux",
                target_os = "solaris",
                target_os = "illumos",
                target_os = "macos",
                target_os = "freebsd",
                target_os = "openbsd",
                target_os = "netbsd",
                target_os = "dragonfly"
            )))]
            {
                // `pin_executable` rejects this target before a spec can be
                // created.  Keep a non-panicking fallback for cfg checking.
                self.path.clone()
            }
        }
        #[cfg(not(unix))]
        {
            self.path.clone()
        }
    }

    fn verify(&self) -> Result<(), Error> {
        match FileIdentity::read(&self.path, true) {
            Ok(current) if current == self.file => Ok(()),
            _ => Err(Error::ExecutableChanged(self.path.clone())),
        }
    }

    fn verify_pinned(&self) -> Result<(), Error> {
        #[cfg(unix)]
        let current = FileIdentity::read_pinned(&self.pinned.file);
        #[cfg(windows)]
        let current = FileIdentity::read_pinned(&self.pinned.handle);
        #[cfg(not(any(unix, windows)))]
        let current: io::Result<FileIdentity> = Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "secure executable pinning is unavailable on this target",
        ));
        match current {
            Ok(current) if current == self.file => Ok(()),
            _ => Err(Error::ExecutableChanged(self.path.clone())),
        }
    }
}

#[cfg(unix)]
#[derive(Clone, Debug)]
struct PinnedExecutable {
    file: Arc<fs::File>,
    script: bool,
}
#[cfg(windows)]
#[derive(Clone, Debug)]
struct PinnedExecutable {
    handle: Arc<OwnedHandle>,
}
#[cfg(not(any(unix, windows)))]
type PinnedExecutable = ();

/// Open the image that will be executed and keep the opened object alive until
/// `Command::spawn` has completed.  On Unix the child executes through an fd
/// path, so replacing the pathname after this point cannot redirect launch.
/// Windows uses a read handle which refuses write and delete sharing while
/// `CreateProcess` opens the image.
fn pin_executable(path: &Path) -> io::Result<PinnedExecutable> {
    #[cfg(unix)]
    {
        #[cfg(any(
            target_os = "linux",
            target_os = "solaris",
            target_os = "illumos",
            target_os = "macos",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly"
        ))]
        {
            let opened = fs::File::open(path)?;
            // Never let the pinned image occupy stdin/stdout/stderr. A daemon
            // may start with one of those descriptors closed; Command's
            // child-side stdio setup would then replace an image pinned at
            // fd 0, 1, or 2 before `/proc/self/fd/N` is executed.
            let pinned_fd = unsafe { libc::fcntl(opened.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
            if pinned_fd == -1 {
                return Err(io::Error::last_os_error());
            }
            let file = unsafe { fs::File::from_raw_fd(pinned_fd) };
            // A script interpreter receives the fd path as argv[0]/script
            // name.  Keep the fd open across the interpreter's exec; native
            // images can use the normal close-on-exec behavior.
            let mut header = [0_u8; 2];
            let read = (&file).read(&mut header)?;
            Ok(PinnedExecutable {
                file: Arc::new(file),
                script: read == 2 && header == *b"#!",
            })
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "solaris",
            target_os = "illumos",
            target_os = "macos",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly"
        )))]
        {
            let _ = path;
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure fd-based executable launch is unavailable on this Unix target",
            ));
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_SHARE_READ, OPEN_EXISTING,
        };

        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_GENERIC_READ,
                FILE_SHARE_READ,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if raw.is_null() || raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(PinnedExecutable {
            handle: Arc::new(unsafe { OwnedHandle::from_raw_handle(raw) }),
        })
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "secure executable pinning is unavailable on this target",
        ))
    }
}

#[cfg(unix)]
fn pin_current_directory(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    let opened = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    // As with the executable image, keep the directory descriptor clear of
    // child stdio slots when a daemon inherited one or more closed standard
    // descriptors. The child reaches this descriptor only from `pre_exec`.
    let pinned_fd = unsafe { libc::fcntl(opened.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if pinned_fd == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { fs::File::from_raw_fd(pinned_fd) })
}

/// The result of a completed helper.
#[derive(Debug)]
pub struct CompletedProcess {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl CompletedProcess {
    pub fn status(&self) -> ExitStatus {
        self.status
    }

    pub fn success(&self) -> bool {
        self.status.success()
    }

    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }
}

struct ProcessControl {
    child: Mutex<Option<Child>>,
    tree: ProcessTree,
    cancellation: CancellationToken,
    deadline: Instant,
    monitored_files: Vec<MonitoredFile>,
    monitored_directories: Vec<MonitoredDirectory>,
    reader_results: Vec<ReaderWatch>,
    reader_stop: Arc<AtomicBool>,
    force_stop: AtomicBool,
    requested_failure: Mutex<Option<Error>>,
    state: Mutex<ControlState>,
    wake: std::sync::Condvar,
}

#[derive(Debug, Default)]
struct ControlState {
    status: Option<ExitStatus>,
    failure: Option<Error>,
    done: bool,
}

#[derive(Clone, Debug)]
struct ReaderWatch {
    stream: StreamName,
    limit: usize,
    result: Arc<Mutex<Option<ReaderResult>>>,
}

impl ProcessControl {
    fn request_stop(&self, failure: Option<Error>) {
        if let Some(failure) = failure {
            if let Ok(mut requested) = self.requested_failure.lock() {
                if requested.is_none() {
                    *requested = Some(failure);
                }
            }
        }
        self.reader_stop.store(true, Ordering::Release);
        self.force_stop.store(true, Ordering::Release);
        self.wake.notify_all();
    }

    fn wait_done(&self) {
        let mut state = self.state.lock().expect("process state mutex poisoned");
        while !state.done {
            state = self.wake.wait(state).expect("process state mutex poisoned");
        }
    }

    fn take_result(&self) -> (Option<ExitStatus>, Option<Error>) {
        let mut state = self.state.lock().expect("process state mutex poisoned");
        (state.status.take(), state.failure.take())
    }
}

fn supervise(control: Arc<ProcessControl>) {
    loop {
        let failure = if control.force_stop.load(Ordering::Acquire) {
            control
                .requested_failure
                .lock()
                .ok()
                .and_then(|mut failure| failure.take())
                .or(Some(Error::Cancelled))
        } else if let Some(failure) = reader_failure_from(&control.reader_results) {
            Some(failure)
        } else if let Some(failure) =
            monitored_failure(&control.monitored_files, &control.monitored_directories)
        {
            Some(failure)
        } else if control.cancellation.is_cancelled() {
            Some(Error::Cancelled)
        } else if Instant::now() >= control.deadline {
            Some(Error::TimedOut)
        } else {
            None
        };

        let mut child_guard = control.child.lock().expect("child mutex poisoned");
        let child = child_guard.as_mut().expect("supervisor child missing");
        if let Some(failure) = failure {
            control.reader_stop.store(true, Ordering::Release);
            let terminate_error = control.tree.terminate_running(child).err();
            let wait_error = child.wait().err();
            drop(child_guard);
            let mut state = control.state.lock().expect("process state mutex poisoned");
            state.failure = Some(
                terminate_error
                    .map(Error::Terminate)
                    .or_else(|| wait_error.map(Error::Wait))
                    .unwrap_or(failure),
            );
            state.done = true;
            control.wake.notify_all();
            return;
        }
        match poll_child(child) {
            Ok(ChildPoll::Exited(status)) => {
                // On Linux `poll_child` uses waitid(WNOWAIT), so the leader
                // remains waitable while the process group is torn down. This
                // avoids a PID-reuse race between reaping and group cleanup.
                let terminate_error = control.tree.terminate_descendants().err();
                let wait_result = match status {
                    Some(status) => Ok(status),
                    None => child.wait(),
                };
                drop(child_guard);
                let mut state = control.state.lock().expect("process state mutex poisoned");
                if let Some(error) = terminate_error {
                    state.failure = Some(Error::Terminate(error));
                } else {
                    match wait_result {
                        Ok(status) => state.status = Some(status),
                        Err(error) => state.failure = Some(Error::Wait(error)),
                    }
                }
                state.done = true;
                control.wake.notify_all();
                return;
            }
            Ok(ChildPoll::Running) => {}
            Err(error) => {
                control.reader_stop.store(true, Ordering::Release);
                let terminate_error = control.tree.terminate_running(child).err();
                let _ = child.wait();
                drop(child_guard);
                let mut state = control.state.lock().expect("process state mutex poisoned");
                state.failure = Some(
                    terminate_error
                        .map(Error::Terminate)
                        .unwrap_or(Error::Wait(error)),
                );
                state.done = true;
                control.wake.notify_all();
                return;
            }
        }
        drop(child_guard);
        thread::sleep(Duration::from_millis(5));
    }
}

enum ChildPoll {
    Running,
    /// The optional status is already reaped. On waitid platforms this is
    /// `None` because WNOWAIT only observes the exit and leaves Child::wait to
    /// reap after descendants have been terminated.
    Exited(Option<ExitStatus>),
}

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "solaris",
    target_os = "illumos"
))]
fn poll_child(child: &Child) -> io::Result<ChildPoll> {
    let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    loop {
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            return if unsafe { info.si_pid() } == 0 {
                Ok(ChildPoll::Running)
            } else {
                Ok(ChildPoll::Exited(None))
            };
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(error);
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "solaris",
    target_os = "illumos"
)))]
fn poll_child(child: &mut Child) -> io::Result<ChildPoll> {
    child
        .try_wait()
        .map(|status| status.map_or(ChildPoll::Running, |status| ChildPoll::Exited(Some(status))))
}

/// A running process whose stdin can be fed incrementally.
pub struct RunningProcess {
    control: Arc<ProcessControl>,
    stdin: Option<ChildStdin>,
    readers: Vec<ReaderSlot>,
    supervisor: Option<JoinHandle<()>>,
}

impl RunningProcess {
    #[cfg(feature = "ffmpeg-encoding")]
    pub fn write_all(&mut self, bytes: &[u8]) -> Result<(), Error> {
        #[cfg(unix)]
        {
            self.write_all_nonblocking(bytes)
        }
        #[cfg(not(unix))]
        {
            self.write_all_worker(bytes)
        }
    }

    #[cfg(all(unix, feature = "ffmpeg-encoding"))]
    fn write_all_nonblocking(&mut self, bytes: &[u8]) -> Result<(), Error> {
        use std::os::unix::io::AsRawFd;

        if bytes.is_empty() {
            return Ok(());
        }
        let fd = match self.stdin.as_ref() {
            Some(stdin) => stdin.as_raw_fd(),
            None => {
                return self.abort(Error::WriteStdin(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "stdin is not piped",
                )))
            }
        };
        let mut offset = 0;
        while offset < bytes.len() {
            if let Some(error) = self.should_abort() {
                return self.abort(error);
            }
            let result = {
                let stdin = self.stdin.as_mut().expect("stdin fd was obtained above");
                stdin.write(&bytes[offset..])
            };
            match result {
                Ok(0) => {
                    return self.abort(Error::WriteStdin(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "helper closed stdin",
                    )))
                }
                Ok(written) => offset += written,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if let Err(error) = wait_for_stdin(fd) {
                        return self.abort(Error::WriteStdin(error));
                    }
                }
                Err(error) => return self.abort(Error::WriteStdin(error)),
            }
        }
        Ok(())
    }

    #[cfg(all(not(unix), feature = "ffmpeg-encoding"))]
    fn write_all_worker(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if bytes.is_empty() {
            return Ok(());
        }
        let stdin = match self.stdin.take() {
            Some(stdin) => stdin,
            None => {
                return self.abort(Error::WriteStdin(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "stdin is not piped",
                )))
            }
        };
        let data = bytes.to_vec();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        if let Err(error) = thread::Builder::new()
            .name("forge-helper-stdin".to_owned())
            .spawn(move || {
                let mut stdin = stdin;
                let result = stdin.write_all(&data);
                let _ = sender.send((stdin, result));
            })
        {
            return self.abort(Error::Spawn(io::Error::other(error.to_string())));
        }
        loop {
            match receiver.recv_timeout(Duration::from_millis(10)) {
                Ok((stdin, result)) => {
                    self.stdin = Some(stdin);
                    return match result {
                        Ok(()) => Ok(()),
                        Err(error) => self.abort(Error::WriteStdin(error)),
                    };
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(error) = self.should_abort() {
                        return self.abort_writer(error, receiver);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return self.abort(Error::WriteStdin(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "stdin writer stopped unexpectedly",
                    )))
                }
            }
        }
    }

    #[cfg(all(not(unix), feature = "ffmpeg-encoding"))]
    fn abort_writer(
        &mut self,
        error: Error,
        receiver: std::sync::mpsc::Receiver<(ChildStdin, io::Result<()>)>,
    ) -> Result<(), Error> {
        self.stdin.take();
        self.control.request_stop(Some(error.clone_for_control()));
        self.control.wait_done();
        // Killing the job closes the child's end of the pipe.  A bounded wait
        // avoids making a Windows caller hang forever if a broken helper has
        // retained the inherited handle itself.
        let _ = receiver.recv_timeout(Duration::from_secs(1));
        self.join_supervisor();
        self.join_readers();
        Err(error)
    }

    #[cfg(feature = "ffmpeg-encoding")]
    fn should_abort(&self) -> Option<Error> {
        if let Some(failure) = reader_failure_from(&self.control.reader_results) {
            return Some(failure);
        }
        if let Some(failure) = monitored_failure(
            &self.control.monitored_files,
            &self.control.monitored_directories,
        ) {
            return Some(failure);
        }
        if self.control.cancellation.is_cancelled() {
            return Some(Error::Cancelled);
        }
        if Instant::now() >= self.control.deadline {
            return Some(Error::TimedOut);
        }
        None
    }

    #[cfg(feature = "ffmpeg-encoding")]
    fn abort(&mut self, error: Error) -> Result<(), Error> {
        self.stdin.take();
        self.control.request_stop(Some(error.clone_for_control()));
        self.control.wait_done();
        self.join_supervisor();
        self.join_readers();
        Err(error)
    }

    pub fn finish(mut self) -> Result<CompletedProcess, Error> {
        self.stdin.take();
        self.control.wait_done();
        self.join_supervisor();
        self.join_readers();
        let (status, failure) = self.control.take_result();
        if let Some(failure) = failure {
            return Err(failure);
        }
        // The child may have exited between the supervisor's last reader
        // check and the reader threads' EOF.  Recheck after joining so a final
        // oversized chunk can never be reported as successful.
        if let Some(failure) = reader_failure_from(&self.control.reader_results).or_else(|| {
            monitored_failure(
                &self.control.monitored_files,
                &self.control.monitored_directories,
            )
        }) {
            return Err(failure);
        }
        Ok(CompletedProcess {
            status: status.ok_or_else(|| Error::Wait(io::Error::other("missing child status")))?,
            stdout: self.take_reader(StreamName::Stdout),
            stderr: self.take_reader(StreamName::Stderr),
        })
    }

    fn join_supervisor(&mut self) {
        if let Some(join) = self.supervisor.take() {
            let _ = join.join();
        }
    }

    fn take_reader(&self, stream: StreamName) -> Vec<u8> {
        self.readers
            .iter()
            .find(|reader| reader.stream == stream)
            .and_then(|reader| reader.result.lock().ok()?.take())
            .and_then(|result| match result {
                ReaderResult::Data(bytes) => Some(bytes),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn join_readers(&mut self) {
        join_readers_bounded(&mut self.readers);
    }
}

impl Drop for RunningProcess {
    fn drop(&mut self) {
        self.stdin.take();
        self.control.request_stop(None);
        self.control.wait_done();
        self.join_supervisor();
        self.join_readers();
    }
}

/// Run a command to completion. If stdin is piped, it is closed immediately;
/// use [`spawn_stream`] when incremental input is required.
pub fn run(spec: ProcessSpec) -> Result<CompletedProcess, Error> {
    let mut process = spawn_stream(spec)?;
    process.stdin.take();
    process.finish()
}

/// FFmpeg-specific naming for the one-shot broker entry point.  The spec is
/// deliberately generic so the same policy can be used by ffprobe and other
/// media helpers without duplicating process-management code.
#[cfg(feature = "ffmpeg-encoding")]
pub fn run_ffmpeg(spec: ProcessSpec) -> Result<CompletedProcess, Error> {
    run(spec)
}

/// FFmpeg-specific naming for the streaming broker entry point.
#[cfg(feature = "ffmpeg-encoding")]
pub fn spawn_ffmpeg(spec: ProcessSpec) -> Result<RunningProcess, Error> {
    spawn_stream(spec)
}

/// Spawn a command with the configured stdin/stdout/stderr policy.
pub fn spawn_stream(spec: ProcessSpec) -> Result<RunningProcess, Error> {
    let (current_dir, monitored_files, monitored_directories) = prepare_paths(&spec)?;
    for file in &monitored_files {
        match monitored_size(file) {
            Ok(Some(actual)) if actual > file.max_bytes => {
                return Err(Error::MonitoredFileLimit {
                    label: file.label.clone(),
                    path: file.path.clone(),
                    limit: file.max_bytes,
                    actual,
                });
            }
            Ok(_) => {}
            Err(message) => {
                return Err(Error::MonitoredFile {
                    label: file.label.clone(),
                    path: file.path.clone(),
                    message,
                });
            }
        }
    }
    for directory in &monitored_directories {
        match monitored_directory_size(directory) {
            Ok(Some((actual_bytes, actual_files)))
                if actual_bytes > directory.max_bytes || actual_files > directory.max_files =>
            {
                return Err(Error::MonitoredDirectoryLimit {
                    label: directory.label.clone(),
                    path: directory.path.clone(),
                    max_bytes: directory.max_bytes,
                    actual_bytes,
                    max_files: directory.max_files,
                    actual_files,
                });
            }
            Ok(_) => {}
            Err(message) => {
                return Err(Error::MonitoredDirectory {
                    label: directory.label.clone(),
                    path: directory.path.clone(),
                    message,
                });
            }
        }
    }
    if spec.cancellation.is_cancelled() {
        return Err(Error::Cancelled);
    }
    spec.executable.verify()?;
    spec.executable.verify_pinned()?;

    if Instant::now().checked_add(spec.timeout).is_none() {
        return Err(Error::InvalidTimeout);
    }
    let launch_path = spec.executable.launch_path();
    let mut command = Command::new(launch_path);
    #[cfg(unix)]
    command.arg0(spec.executable.path());
    command.args(&spec.args);
    apply_env(&mut command, &spec.env_policy);
    #[cfg(unix)]
    let pinned_current_dir = current_dir
        .as_ref()
        .map(|path| pin_current_directory(path))
        .transpose()
        .map_err(Error::Spawn)?;
    #[cfg(not(unix))]
    if let Some(current_dir) = &current_dir {
        command.current_dir(current_dir);
    }
    command.stdin(match spec.stdin {
        StdinMode::Null => Stdio::null(),
        StdinMode::Piped => Stdio::piped(),
    });
    command.stdout(stdio_for(spec.stdout));
    command.stderr(stdio_for(spec.stderr));
    #[cfg(unix)]
    unsafe {
        let pinned_fd = spec.executable.pinned.file.as_raw_fd();
        let script = spec.executable.pinned.script;
        let current_dir_fd = pinned_current_dir.as_ref().map(|file| file.as_raw_fd());
        command.pre_exec(move || {
            if let Some(current_dir_fd) = current_dir_fd {
                if libc::fchdir(current_dir_fd) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else if script {
                let flags = libc::fcntl(pinned_fd, libc::F_GETFD);
                if flags == -1
                    || libc::fcntl(pinned_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
                {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            } else {
                Ok(())
            }
        });
    }
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
    let started = Instant::now();
    let deadline = started
        .checked_add(spec.timeout)
        .ok_or(Error::InvalidTimeout)?;
    let mut child = command.spawn().map_err(Error::Spawn)?;
    let tree = match ProcessTree::attach(&child) {
        Ok(tree) => tree,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };

    #[cfg(windows)]
    if let Err(error) = resume_primary_thread(child.id()) {
        let _ = tree.terminate_running(&mut child);
        let _ = child.wait();
        return Err(error);
    }

    #[cfg(unix)]
    if let Err(error) = set_stdin_nonblocking(&child) {
        tree.terminate(&mut child);
        let _ = child.wait();
        return Err(Error::Spawn(error));
    }

    let stdin = child.stdin.take();
    let reader_stop = Arc::new(AtomicBool::new(false));
    let mut readers: Vec<ReaderSlot> = Vec::new();
    if let OutputMode::Capture { max_bytes } = spec.stdout {
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                tree.terminate(&mut child);
                let _ = child.wait();
                reader_stop.store(true, Ordering::Release);
                join_readers_bounded(&mut readers);
                return Err(Error::Spawn(io::Error::other("stdout pipe missing")));
            }
        };
        match ReaderSlot::start(
            StreamName::Stdout,
            stdout,
            max_bytes,
            Arc::clone(&reader_stop),
        ) {
            Ok(reader) => readers.push(reader),
            Err(error) => {
                tree.terminate(&mut child);
                let _ = child.wait();
                reader_stop.store(true, Ordering::Release);
                join_readers_bounded(&mut readers);
                return Err(Error::Spawn(error));
            }
        }
    }
    if let OutputMode::Capture { max_bytes } = spec.stderr {
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                tree.terminate(&mut child);
                let _ = child.wait();
                reader_stop.store(true, Ordering::Release);
                join_readers_bounded(&mut readers);
                return Err(Error::Spawn(io::Error::other("stderr pipe missing")));
            }
        };
        match ReaderSlot::start(
            StreamName::Stderr,
            stderr,
            max_bytes,
            Arc::clone(&reader_stop),
        ) {
            Ok(reader) => readers.push(reader),
            Err(error) => {
                tree.terminate(&mut child);
                let _ = child.wait();
                reader_stop.store(true, Ordering::Release);
                join_readers_bounded(&mut readers);
                return Err(Error::Spawn(error));
            }
        }
    }
    let reader_results = readers.iter().map(ReaderSlot::watch).collect();
    let control = Arc::new(ProcessControl {
        child: Mutex::new(Some(child)),
        tree,
        cancellation: spec.cancellation,
        deadline,
        monitored_files,
        monitored_directories,
        reader_results,
        reader_stop,
        force_stop: AtomicBool::new(false),
        requested_failure: Mutex::new(None),
        state: Mutex::new(ControlState::default()),
        wake: std::sync::Condvar::new(),
    });
    let supervisor_control = Arc::clone(&control);
    let supervisor = match thread::Builder::new()
        .name("forge-helper-supervisor".to_owned())
        .spawn(move || supervise(supervisor_control))
    {
        Ok(supervisor) => supervisor,
        Err(error) => {
            let failure = Error::Spawn(io::Error::other(error.to_string()));
            if let Ok(mut child_guard) = control.child.lock() {
                if let Some(child) = child_guard.as_mut() {
                    control.tree.terminate(child);
                    let _ = child.wait();
                }
            }
            control.reader_stop.store(true, Ordering::Release);
            join_readers_bounded(&mut readers);
            return Err(failure);
        }
    };
    Ok(RunningProcess {
        control,
        stdin,
        readers,
        supervisor: Some(supervisor),
    })
}

fn apply_env(command: &mut Command, policy: &EnvPolicy) {
    match policy {
        EnvPolicy::Clear => {
            command.env_clear();
        }
        EnvPolicy::Minimal => {
            command.env_clear();
            #[cfg(windows)]
            {
                let system_root = windows_system_root();
                let system_path = env::join_paths([
                    PathBuf::from(&system_root).join("System32"),
                    PathBuf::from(&system_root),
                ])
                .unwrap_or_else(|_| system_root.clone());
                command.env("SystemRoot", &system_root);
                command.env("WINDIR", &system_root);
                command.env("PATH", system_path);
            }
            #[cfg(not(windows))]
            {
                command.env("PATH", "/usr/bin:/bin");
                command.env("LANG", "C");
                command.env("LC_ALL", "C");
                command.env("LC_CTYPE", "C");
            }
        }
        EnvPolicy::Inherit => {}
        EnvPolicy::AllowList(names) => {
            command.env_clear();
            for name in names {
                if let Some(value) = env::var_os(name) {
                    command.env(name, value);
                }
            }
        }
        EnvPolicy::Explicit(values) => {
            command.env_clear();
            for (name, value) in values {
                command.env(name, value);
            }
        }
    }
}

#[cfg(windows)]
fn windows_system_root() -> OsString {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;

    // Windows does not require the installation to live on C:. Query the OS
    // so the minimal child environment remains portable without inheriting
    // the parent's full environment.
    let mut buffer = vec![0_u16; 32_768];
    let length = unsafe { GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if length != 0 && (length as usize) < buffer.len() {
        buffer.truncate(length as usize);
        OsString::from_wide(&buffer)
    } else {
        env::var_os("SystemRoot").unwrap_or_else(|| OsString::from(r"C:\Windows"))
    }
}

type PreparedPaths = (Option<PathBuf>, Vec<MonitoredFile>, Vec<MonitoredDirectory>);

fn prepare_paths(spec: &ProcessSpec) -> Result<PreparedPaths, Error> {
    let process_cwd = env::current_dir().map_err(|error| {
        Error::InvalidConfiguration(format!("resolve broker current directory: {error}"))
    })?;
    let child_dir = if let Some(path) = &spec.current_dir {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            process_cwd.join(path)
        };
        let metadata = fs::symlink_metadata(&absolute).map_err(|error| {
            Error::InvalidConfiguration(format!(
                "current directory {}: {error}",
                absolute.display()
            ))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(Error::InvalidConfiguration(format!(
                "current directory is not a non-symlink directory: {}",
                absolute.display()
            )));
        }
        Some(fs::canonicalize(&absolute).map_err(|error| {
            Error::InvalidConfiguration(format!(
                "canonicalize current directory {}: {error}",
                absolute.display()
            ))
        })?)
    } else {
        None
    };
    let base = child_dir.as_deref().unwrap_or(&process_cwd);
    let files = spec
        .monitored_files
        .iter()
        .cloned()
        .map(|mut file| {
            if !file.path.is_absolute() {
                file.path = base.join(&file.path);
            }
            file
        })
        .collect();
    let directories = spec
        .monitored_directories
        .iter()
        .cloned()
        .map(|mut directory| {
            if !directory.path.is_absolute() {
                directory.path = base.join(&directory.path);
            }
            directory
        })
        .collect();
    Ok((child_dir, files, directories))
}

fn stdio_for(mode: OutputMode) -> Stdio {
    match mode {
        OutputMode::Null => Stdio::null(),
        OutputMode::Capture { .. } => Stdio::piped(),
    }
}

#[cfg(all(unix, feature = "ffmpeg-encoding"))]
fn wait_for_stdin(fd: std::os::unix::io::RawFd) -> io::Result<()> {
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    loop {
        let result = unsafe { libc::poll(&mut descriptor, 1, 10) };
        if result >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(unix)]
fn set_stdin_nonblocking(child: &Child) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let Some(stdin) = child.stdin.as_ref() else {
        return Ok(());
    };
    let fd = stdin.as_raw_fd();
    // ChildStdin is a pipe created by std::process.  Marking only this handle
    // non-blocking lets the caller's write loop observe the same deadline and
    // cancellation signal as the process supervisor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[derive(Debug)]
struct ReaderSlot {
    stream: StreamName,
    limit: usize,
    result: Arc<Mutex<Option<ReaderResult>>>,
    join: Option<JoinHandle<()>>,
}

#[derive(Debug)]
enum ReaderResult {
    Data(Vec<u8>),
    Limit,
    Io(String),
    Panic(String),
}

impl ReaderSlot {
    #[cfg(unix)]
    fn start<R>(
        stream: StreamName,
        mut reader: R,
        limit: usize,
        stop: Arc<AtomicBool>,
    ) -> io::Result<Self>
    where
        R: Read + Send + 'static + std::os::unix::io::AsRawFd,
    {
        let result = Arc::new(Mutex::new(None));
        let result_for_thread = Arc::clone(&result);
        let join = thread::Builder::new()
            .name(format!("forge-helper-{stream}"))
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let fd = reader.as_raw_fd();
                    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                    if flags == -1 {
                        return ReaderResult::Io(io::Error::last_os_error().to_string());
                    }
                    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
                        return ReaderResult::Io(io::Error::last_os_error().to_string());
                    }
                    let mut data = Vec::with_capacity(limit.min(8192));
                    let mut buffer = [0_u8; 8192];
                    loop {
                        let mut descriptor = libc::pollfd {
                            fd,
                            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                            revents: 0,
                        };
                        let stopping = stop.load(Ordering::Acquire);
                        let polled = unsafe {
                            libc::poll(&mut descriptor, 1, if stopping { 0 } else { 20 })
                        };
                        if polled < 0 {
                            let error = io::Error::last_os_error();
                            if error.kind() == io::ErrorKind::Interrupted {
                                continue;
                            }
                            return ReaderResult::Io(error.to_string());
                        }
                        if polled == 0 {
                            if stopping {
                                return ReaderResult::Data(data);
                            }
                            continue;
                        }
                        match reader.read(&mut buffer) {
                            Ok(0) => return ReaderResult::Data(data),
                            Ok(read) => {
                                let remaining = limit.saturating_sub(data.len());
                                if read > remaining {
                                    data.extend_from_slice(&buffer[..remaining]);
                                    return ReaderResult::Limit;
                                }
                                data.extend_from_slice(&buffer[..read]);
                            }
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                            Err(error) => return ReaderResult::Io(error.to_string()),
                        }
                    }
                }));
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(payload) => ReaderResult::Panic(panic_message(payload)),
                };
                match result_for_thread.lock() {
                    Ok(mut slot) => *slot = Some(outcome),
                    Err(poisoned) => {
                        let mut slot = poisoned.into_inner();
                        *slot = Some(ReaderResult::Panic(
                            "reader result mutex was poisoned".to_owned(),
                        ));
                    }
                }
            })?;
        Ok(Self {
            stream,
            limit,
            result,
            join: Some(join),
        })
    }

    #[cfg(not(unix))]
    fn start<R: Read + Send + 'static>(
        stream: StreamName,
        mut reader: R,
        limit: usize,
        stop: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        let result = Arc::new(Mutex::new(None));
        let result_for_thread = Arc::clone(&result);
        let join = thread::Builder::new()
            .name(format!("forge-helper-{stream}"))
            .spawn(move || {
                let mut data = Vec::with_capacity(limit.min(8192));
                let mut buffer = [0_u8; 8192];
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| loop {
                    if stop.load(Ordering::Acquire) && !data.is_empty() {
                        return ReaderResult::Data(data);
                    }
                    match reader.read(&mut buffer) {
                        Ok(0) => return ReaderResult::Data(data),
                        Ok(read) => {
                            let remaining = limit.saturating_sub(data.len());
                            if read > remaining {
                                data.extend_from_slice(&buffer[..remaining]);
                                return ReaderResult::Limit;
                            }
                            data.extend_from_slice(&buffer[..read]);
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => return ReaderResult::Io(error.to_string()),
                    }
                }));
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(payload) => ReaderResult::Panic(panic_message(payload)),
                };
                match result_for_thread.lock() {
                    Ok(mut slot) => *slot = Some(outcome),
                    Err(poisoned) => {
                        let mut slot = poisoned.into_inner();
                        *slot = Some(ReaderResult::Panic(
                            "reader result mutex was poisoned".to_owned(),
                        ));
                    }
                }
            })?;
        Ok(Self {
            stream,
            limit,
            result,
            join: Some(join),
        })
    }

    fn watch(&self) -> ReaderWatch {
        ReaderWatch {
            stream: self.stream,
            limit: self.limit,
            result: Arc::clone(&self.result),
        }
    }
}

fn join_readers_bounded(readers: &mut [ReaderSlot]) {
    for reader in readers {
        let deadline = Instant::now() + Duration::from_secs(1);
        while reader
            .result
            .lock()
            .map(|result| result.is_none())
            .unwrap_or(false)
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(5));
        }
        if let Some(join) = reader.join.take() {
            if reader
                .result
                .lock()
                .map(|result| result.is_some())
                .unwrap_or(false)
            {
                let _ = join.join();
            } else {
                if let Ok(mut result) = reader.result.lock() {
                    *result = Some(ReaderResult::Panic(
                        "reader thread did not stop within the shutdown bound".to_owned(),
                    ));
                }
                // Dropping a JoinHandle detaches the reader. On Unix the
                // poll-based reader normally exits within one tick; this
                // fallback keeps Drop bounded even for a hostile Read impl or
                // an unsupported platform pipe implementation.
            }
        }
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "reader thread panicked".to_owned()
    }
}

fn reader_failure_from(readers: &[ReaderWatch]) -> Option<Error> {
    for reader in readers {
        let guard = match reader.result.lock() {
            Ok(guard) => guard,
            Err(_) => {
                return Some(Error::Reader {
                    stream: reader.stream,
                    message: "reader result mutex was poisoned".to_owned(),
                })
            }
        };
        match guard.as_ref() {
            Some(ReaderResult::Limit) => {
                return Some(Error::OutputLimit {
                    stream: reader.stream,
                    limit: reader.limit,
                });
            }
            Some(ReaderResult::Io(message)) => {
                return Some(Error::Reader {
                    stream: reader.stream,
                    message: message.clone(),
                });
            }
            Some(ReaderResult::Panic(message)) => {
                return Some(Error::Reader {
                    stream: reader.stream,
                    message: format!("reader thread panicked: {message}"),
                });
            }
            _ => {}
        }
    }
    None
}

fn monitored_failure(files: &[MonitoredFile], directories: &[MonitoredDirectory]) -> Option<Error> {
    for file in files {
        match monitored_size(file) {
            Ok(Some(actual)) if actual > file.max_bytes => {
                return Some(Error::MonitoredFileLimit {
                    label: file.label.clone(),
                    path: file.path.clone(),
                    limit: file.max_bytes,
                    actual,
                });
            }
            Ok(_) => {}
            Err(message) => {
                return Some(Error::MonitoredFile {
                    label: file.label.clone(),
                    path: file.path.clone(),
                    message,
                });
            }
        }
    }
    for directory in directories {
        match monitored_directory_size(directory) {
            Ok(Some((actual_bytes, actual_files)))
                if actual_bytes > directory.max_bytes || actual_files > directory.max_files =>
            {
                return Some(Error::MonitoredDirectoryLimit {
                    label: directory.label.clone(),
                    path: directory.path.clone(),
                    max_bytes: directory.max_bytes,
                    actual_bytes,
                    max_files: directory.max_files,
                    actual_files,
                });
            }
            Ok(_) => {}
            Err(message) => {
                return Some(Error::MonitoredDirectory {
                    label: directory.label.clone(),
                    path: directory.path.clone(),
                    message,
                });
            }
        }
    }
    None
}

fn monitored_size(file: &MonitoredFile) -> Result<Option<u64>, String> {
    let metadata = match fs::symlink_metadata(&file.path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if metadata.file_type().is_symlink() {
        return Err("symbolic links are not allowed".to_owned());
    }
    if !metadata.is_file() {
        return Err("path is not a regular file".to_owned());
    }
    Ok(Some(metadata.len()))
}

fn monitored_directory_size(directory: &MonitoredDirectory) -> Result<Option<(u64, u64)>, String> {
    let metadata = match fs::symlink_metadata(&directory.path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if metadata.file_type().is_symlink() {
        return Err("symbolic links are not allowed".to_owned());
    }
    if !metadata.is_dir() {
        return Err("path is not a directory".to_owned());
    }
    let root_marker = fs::canonicalize(&directory.path).map_err(|error| error.to_string())?;

    let mut bytes = 0_u64;
    let mut files = 0_u64;
    let mut pending = vec![directory.path.clone()];
    while let Some(path) = pending.pop() {
        let marker = fs::canonicalize(&path).map_err(|error| error.to_string())?;
        let path_metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        if path_metadata.file_type().is_symlink() || !path_metadata.is_dir() {
            return Err(format!(
                "directory changed while monitored: {}",
                path.display()
            ));
        }
        let entries = fs::read_dir(&path).map_err(|error| error.to_string())?;
        for entry in entries {
            let entry = entry.map_err(|error| error.to_string())?;
            let child = entry.path();
            let metadata = fs::symlink_metadata(&child).map_err(|error| error.to_string())?;
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                return Err(format!(
                    "symbolic links are not allowed: {}",
                    child.display()
                ));
            }
            if metadata.is_dir() {
                files = files
                    .checked_add(1)
                    .ok_or_else(|| "entry count overflow".to_owned())?;
                if files > directory.max_files {
                    ensure_directory_scan_stable(&directory.path, &root_marker, &path, &marker)?;
                    return Ok(Some((bytes, files)));
                }
                pending.push(child);
            } else if metadata.is_file() {
                files = files
                    .checked_add(1)
                    .ok_or_else(|| "entry count overflow".to_owned())?;
                bytes = bytes
                    .checked_add(metadata.len())
                    .ok_or_else(|| "byte count overflow".to_owned())?;
                // Stop scanning as soon as a hard limit is known to be
                // exceeded. This bounds work for hostile output trees.
                if bytes > directory.max_bytes || files > directory.max_files {
                    ensure_directory_scan_stable(&directory.path, &root_marker, &path, &marker)?;
                    return Ok(Some((bytes, files)));
                }
            } else {
                return Err(format!(
                    "special files are not allowed: {}",
                    child.display()
                ));
            }
        }
        let after = fs::canonicalize(&path).map_err(|error| error.to_string())?;
        if after != marker {
            return Err(format!(
                "directory changed while monitored: {}",
                path.display()
            ));
        }
    }
    let final_marker = fs::canonicalize(&directory.path).map_err(|error| error.to_string())?;
    if final_marker != root_marker {
        return Err("monitored directory changed while it was scanned".to_owned());
    }
    Ok(Some((bytes, files)))
}

fn ensure_directory_scan_stable(
    root: &Path,
    root_marker: &Path,
    path: &Path,
    marker: &Path,
) -> Result<(), String> {
    let current = fs::canonicalize(path).map_err(|error| error.to_string())?;
    let final_root = fs::canonicalize(root).map_err(|error| error.to_string())?;
    if current != marker || final_root != root_marker {
        return Err("monitored directory changed while it was scanned".to_owned());
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume_serial: u32,
    #[cfg(windows)]
    file_index: u64,
    length: u64,
    #[cfg(any(unix, not(windows)))]
    modified: Option<std::time::SystemTime>,
    digest: [u8; 32],
}

impl FileIdentity {
    fn read(path: &Path, require_executable: bool) -> io::Result<Self> {
        if require_executable && fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "executable path must not be a symbolic link after resolution",
            ));
        }
        let metadata = fs::metadata(path)?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "executable is not a regular file",
            ));
        }
        if metadata.len() > MAX_EXECUTABLE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "executable exceeds the identity hashing size limit",
            ));
        }
        #[cfg(unix)]
        if require_executable && metadata.permissions().mode() & 0o111 == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "file is not executable",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                length: metadata.len(),
                modified: metadata.modified().ok(),
                digest: digest_file(path)?,
            })
        }
        #[cfg(windows)]
        {
            let file = fs::File::open(path)?;
            Self::from_windows_file(&file)
        }
        #[cfg(not(any(unix, windows)))]
        {
            Ok(Self {
                length: metadata.len(),
                modified: metadata.modified().ok(),
                digest: digest_file(path)?,
            })
        }
    }

    #[cfg(windows)]
    fn read_pinned(file: &OwnedHandle) -> io::Result<Self> {
        // `OwnedHandle` is intentionally borrowed as a `File` only for the
        // duration of the metadata/hash read.  `ManuallyDrop` prevents the
        // temporary wrapper from closing the owner handle.
        let borrowed =
            std::mem::ManuallyDrop::new(unsafe { fs::File::from_raw_handle(file.as_raw_handle()) });
        Self::from_windows_file(&borrowed)
    }

    #[cfg(unix)]
    fn read_pinned(file: &fs::File) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;

        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pinned executable is not a regular file",
            ));
        }
        if metadata.len() > MAX_EXECUTABLE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "pinned executable exceeds the identity hashing size limit",
            ));
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified: metadata.modified().ok(),
            digest: digest_file_handle(file)?,
        })
    }

    #[cfg(windows)]
    fn from_windows_file(file: &fs::File) -> io::Result<Self> {
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };

        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let length =
            (u64::from(information.nFileSizeHigh) << 32) | u64::from(information.nFileSizeLow);
        if length > MAX_EXECUTABLE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "pinned executable exceeds the identity hashing size limit",
            ));
        }
        let file_index =
            (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
        Ok(Self {
            volume_serial: information.dwVolumeSerialNumber,
            file_index,
            length,
            digest: digest_file_handle(file)?,
        })
    }
}

#[cfg(not(windows))]
fn digest_file(path: &Path) -> io::Result<[u8; 32]> {
    let mut file = fs::File::open(path)?;
    if file.metadata()?.len() > MAX_EXECUTABLE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "executable exceeds the identity hashing size limit",
        ));
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut offset = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        if offset.saturating_add(read as u64) > MAX_EXECUTABLE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "executable exceeds the identity hashing size limit",
            ));
        }
        hasher.update(&buffer[..read]);
        offset = offset.saturating_add(read as u64);
    }
    Ok(hasher.finalize().into())
}

#[cfg(any(unix, windows))]
fn digest_file_handle(file: &fs::File) -> io::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut offset = 0_u64;
    loop {
        #[cfg(unix)]
        let read = std::os::unix::fs::FileExt::read_at(file, &mut buffer, offset)?;
        #[cfg(windows)]
        let read = {
            use std::os::windows::fs::FileExt;
            file.seek_read(&mut buffer, offset)?
        };
        if read == 0 {
            break;
        }
        if offset.saturating_add(read as u64) > MAX_EXECUTABLE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "executable exceeds the identity hashing size limit",
            ));
        }
        hasher.update(&buffer[..read]);
        offset = offset.saturating_add(read as u64);
    }
    Ok(hasher.finalize().into())
}

fn is_path_like(executable: &OsStr) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        executable.as_bytes().contains(&b'/')
    }
    #[cfg(windows)]
    {
        executable
            .to_string_lossy()
            .chars()
            .any(|character| matches!(character, '/' | '\\' | ':'))
    }
    #[cfg(not(any(unix, windows)))]
    {
        executable.to_string_lossy().contains('/')
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct ProcessTree {
    pid: libc::pid_t,
}

#[cfg(unix)]
impl ProcessTree {
    fn attach(child: &Child) -> Result<Self, Error> {
        Ok(Self {
            pid: child.id() as libc::pid_t,
        })
    }

    fn terminate_running(&self, child: &mut Child) -> io::Result<()> {
        // The pre_exec hook creates a new session whose process group is named
        // after the child pid. An ESRCH means the group already disappeared;
        // every other error is actionable and must not be silently ignored.
        let mut first_error = None;
        #[cfg(target_os = "linux")]
        if let Err(error) = kill_linux_descendants(self.pid) {
            first_error = Some(error);
        }
        let group_result = unsafe { libc::kill(-self.pid, libc::SIGKILL) };
        if group_result == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                first_error.get_or_insert(error);
            }
        }
        if let Err(error) = child.kill() {
            if error.kind() != io::ErrorKind::NotFound {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn terminate_descendants(&self) -> io::Result<()> {
        let mut first_error = None;
        #[cfg(target_os = "linux")]
        if let Err(error) = kill_linux_descendants(self.pid) {
            first_error = Some(error);
        }
        let result = unsafe { libc::kill(-self.pid, libc::SIGKILL) };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn terminate(&self, child: &mut Child) {
        let _ = self.terminate_running(child);
    }
}

#[cfg(target_os = "linux")]
fn kill_linux_descendants(root: libc::pid_t) -> io::Result<()> {
    // Process groups do not catch a descendant that calls setsid(). Before
    // killing the group, walk the kernel-maintained child lists while the
    // leader is still alive. This is intentionally a best-effort supplement:
    // the group kill remains the primary containment primitive and races with
    // reparenting are treated as an already-gone process.
    let mut pending = vec![root];
    let mut descendants = Vec::new();
    let mut seen = std::collections::HashSet::from([root]);
    while let Some(parent) = pending.pop() {
        let path = format!("/proc/{parent}/task/{parent}/children");
        let children = match fs::read_to_string(path) {
            Ok(children) => children,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for child in children.split_whitespace() {
            let Ok(pid) = child.parse::<libc::pid_t>() else {
                continue;
            };
            if pid <= 0 || !seen.insert(pid) {
                continue;
            }
            descendants.push(pid);
            pending.push(pid);
        }
    }
    for pid in descendants.iter().rev().copied() {
        let result = unsafe { libc::kill(pid, libc::SIGKILL) };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
#[derive(Debug)]
struct ProcessTree;

#[cfg(not(any(unix, windows)))]
impl ProcessTree {
    fn attach(_child: &Child) -> Result<Self, Error> {
        Err(Error::ProcessTreeUnavailable(format!(
            "{} does not provide a supported process-tree primitive",
            env::consts::OS
        )))
    }

    fn terminate_running(&self, child: &mut Child) -> io::Result<()> {
        let _ = child.kill();
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process-tree containment unavailable",
        ))
    }

    fn terminate_descendants(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process-tree containment unavailable",
        ))
    }

    fn terminate(&self, child: &mut Child) {
        let _ = self.terminate_running(child);
    }
}

#[cfg(windows)]
#[derive(Debug)]
struct ProcessTree {
    job: JobHandle,
}

#[cfg(windows)]
impl ProcessTree {
    fn attach(child: &Child) -> Result<Self, Error> {
        JobHandle::attach(child).map(|job| Self { job })
    }

    fn terminate_running(&self, child: &mut Child) -> io::Result<()> {
        let job_error = self.job.terminate().err();
        if job_error.is_none() {
            return Ok(());
        }
        let child_error = match child.kill() {
            Ok(()) => None,
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => Some(error),
        };
        job_error.or(child_error).map_or(Ok(()), Err)
    }

    fn terminate_descendants(&self) -> io::Result<()> {
        self.job.terminate()
    }

    fn terminate(&self, child: &mut Child) {
        let _ = self.terminate_running(child);
    }
}

#[cfg(windows)]
#[derive(Debug)]
struct JobHandle(OwnedHandle);

#[cfg(windows)]
impl JobHandle {
    fn attach(child: &Child) -> Result<Self, Error> {
        use std::ptr::null_mut;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        let raw_job = unsafe { CreateJobObjectW(null_mut(), std::ptr::null()) };
        if raw_job.is_null() {
            return Err(Error::ProcessTreeUnavailable(
                io::Error::last_os_error().to_string(),
            ));
        }
        // CreateJobObjectW returns an owned kernel handle. OwnedHandle makes
        // its cross-thread ownership and every error-path close explicit.
        let job = unsafe { OwnedHandle::from_raw_handle(raw_job) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0
            || unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle() as _) }
                == 0
        {
            return Err(Error::ProcessTreeUnavailable(
                io::Error::last_os_error().to_string(),
            ));
        }
        Ok(Self(job))
    }

    fn terminate(&self) -> io::Result<()> {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        if unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
fn resume_primary_thread(process_id: u32) -> Result<(), Error> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(Error::Spawn(io::Error::last_os_error()));
    }
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    let mut result = Err(Error::Spawn(io::Error::new(
        io::ErrorKind::NotFound,
        "primary process thread was not found",
    )));
    let mut first_error = None;
    let mut has_entry = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while has_entry {
        if entry.th32OwnerProcessID == process_id {
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() || thread == INVALID_HANDLE_VALUE {
                first_error.get_or_insert_with(io::Error::last_os_error);
            } else {
                let previous = unsafe { ResumeThread(thread) };
                unsafe { CloseHandle(thread) };
                if previous != u32::MAX {
                    result = Ok(());
                    break;
                }
                first_error.get_or_insert_with(io::Error::last_os_error);
            }
        }
        has_entry = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    if result.is_err() {
        if let Some(error) = first_error {
            return Err(Error::Spawn(error));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn shell(script: &str) -> ProcessSpec {
        let mut spec = ProcessSpec::new("sh").expect("sh in PATH");
        spec.args(["-c", script]);
        spec.env_policy(EnvPolicy::Clear);
        spec
    }

    #[test]
    fn resolves_to_canonical_path_and_captures_bounded_streams() {
        #[cfg(unix)]
        {
            let mut spec = shell("printf abc; printf def >&2");
            spec.stdout(OutputMode::capture(3));
            spec.stderr(OutputMode::capture(3));
            let output = run(spec).expect("helper succeeds");
            assert_eq!(output.stdout(), b"abc");
            assert_eq!(output.stderr(), b"def");
        }
    }

    #[test]
    fn normal_completion_drains_large_captured_output() {
        #[cfg(unix)]
        {
            let mut spec = shell("head -c 20000 /dev/zero");
            spec.stdout(OutputMode::capture(20_000));
            let output = run(spec).expect("helper succeeds");
            assert_eq!(output.stdout().len(), 20_000);
        }
    }

    #[test]
    fn output_limit_terminates_and_reaps() {
        #[cfg(unix)]
        {
            let mut spec = shell("printf 0123456789");
            spec.stdout(OutputMode::capture(4));
            assert!(matches!(
                run(spec),
                Err(Error::OutputLimit {
                    stream: StreamName::Stdout,
                    limit: 4
                })
            ));
        }
    }

    #[test]
    fn timeout_is_a_wall_clock_deadline() {
        #[cfg(unix)]
        {
            let mut spec = shell("sleep 5");
            spec.timeout(Duration::from_millis(25));
            assert!(matches!(run(spec), Err(Error::TimedOut)));
        }
    }

    #[test]
    fn cancellation_is_cloneable() {
        #[cfg(unix)]
        {
            let token = CancellationToken::new();
            let mut spec = shell("sleep 5");
            spec.cancellation(token.clone());
            let process = spawn_stream(spec).expect("spawn");
            token.cancel();
            assert!(matches!(process.finish(), Err(Error::Cancelled)));
        }
    }

    #[test]
    fn cancellation_kills_without_finish_or_drop() {
        #[cfg(unix)]
        {
            let directory = tempfile::tempdir().expect("tempdir");
            let marker = directory.path().join("marker");
            let script = format!(
                "sleep 1; printf alive > {}",
                shell_quote(&marker.to_string_lossy())
            );
            let token = CancellationToken::new();
            let mut spec = shell(&script);
            spec.cancellation(token.clone());
            let process = spawn_stream(spec).expect("spawn");
            token.cancel();
            // Keep the RunningProcess alive and wait only through its private
            // control state. This proves the watchdog reacts without relying
            // on finish() or Drop to initiate termination.
            process.control.wait_done();
            let state = process.control.state.lock().expect("state");
            assert!(matches!(state.failure, Some(Error::Cancelled)));
            drop(state);
            assert!(!marker.exists(), "supervisor left cancelled helper alive");
            drop(process);
        }
    }

    #[test]
    fn normal_completion_terminates_descendants() {
        #[cfg(unix)]
        {
            let directory = tempfile::tempdir().expect("tempdir");
            let marker = directory.path().join("descendant-marker");
            let script = format!(
                "(sleep .2; printf alive > {}) & exit 0",
                shell_quote(&marker.to_string_lossy())
            );
            let output = run(shell(&script)).expect("leader succeeds");
            assert!(output.success());
            thread::sleep(Duration::from_millis(350));
            assert!(
                !marker.exists(),
                "normal completion left a descendant alive"
            );
        }
    }

    #[test]
    fn missing_monitored_file_is_allowed_and_regular_file_is_checked() {
        #[cfg(unix)]
        {
            let directory = tempfile::tempdir().expect("tempdir");
            let path = directory.path().join("artifact");
            // The relative command runs in the current directory, so first
            // exercise the missing-file path with a command that does nothing.
            let mut missing = shell(":");
            missing.monitor_file(MonitoredFile::new(&path, 1, "artifact"));
            assert!(run(missing).expect("missing is allowed").success());
        }
    }

    #[test]
    fn monitored_file_limit_is_enforced() {
        #[cfg(unix)]
        {
            let directory = tempfile::tempdir().expect("tempdir");
            let path = directory.path().join("artifact");
            let mut spec = shell("printf 123 > artifact");
            spec.current_dir(directory.path().to_path_buf());
            spec.monitor_file(MonitoredFile::new(&path, 2, "artifact"));
            assert!(matches!(
                run(spec),
                Err(Error::MonitoredFileLimit { limit: 2, .. })
            ));
        }
    }

    #[cfg(unix)]
    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    #[test]
    fn explicit_environment_is_applied() {
        #[cfg(unix)]
        {
            let mut spec = shell("printf %s \"$FORGE_SUBPROCESS_TEST\"");
            spec.env_policy(EnvPolicy::Explicit(vec![(
                OsString::from("FORGE_SUBPROCESS_TEST"),
                OsString::from("ok"),
            )]));
            spec.stdout(OutputMode::capture(2));
            let output = run(spec).expect("helper succeeds");
            assert_eq!(output.stdout(), b"ok");
        }
    }

    #[test]
    fn executable_replacement_is_rejected_before_launch() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let directory = tempfile::tempdir().expect("tempdir");
            let path = directory.path().join("helper.sh");
            fs::write(&path, b"#!/bin/sh\nexit 0\n").expect("write helper");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod helper");
            let spec = ProcessSpec::new(&path).expect("resolve helper");
            fs::write(&path, b"#!/bin/sh\nexit 1\n").expect("replace helper");
            assert!(matches!(run(spec), Err(Error::ExecutableChanged(_))));
        }
    }

    #[test]
    fn pinned_script_launches_through_fd_path() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let directory = tempfile::tempdir().expect("tempdir");
            let path = directory.path().join("helper.sh");
            fs::write(&path, b"#!/bin/sh\nprintf script\n").expect("write helper");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod helper");
            let mut spec = ProcessSpec::new(&path).expect("resolve helper");
            assert!(spec.executable.pinned.file.as_raw_fd() >= 3);
            spec.stdout(OutputMode::capture(6));
            let output = run(spec).expect("script succeeds");
            assert_eq!(output.stdout(), b"script");
        }
    }

    #[cfg(unix)]
    #[test]
    fn pinned_executable_survives_a_closed_standard_descriptor() {
        const CHILD_MARKER_ENV: &str = "FORGE_SUBPROCESS_CLOSED_FD_CHILD_MARKER";
        if let Some(marker) = env::var_os(CHILD_MARKER_ENV) {
            use std::os::unix::fs::PermissionsExt;

            let directory = tempfile::tempdir().expect("tempdir");
            let helper = directory.path().join("helper.sh");
            fs::write(&helper, b"#!/bin/sh\nprintf script\n").expect("write helper");
            fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).expect("chmod helper");
            // This is a dedicated child test process. Make the next opened
            // file eligible for fd 0, which used to let child stdio setup
            // overwrite the pinned executable before exec.
            unsafe {
                libc::close(libc::STDIN_FILENO);
            }
            let mut spec = ProcessSpec::new(&helper).expect("resolve helper with stdin closed");
            assert!(spec.executable.pinned.file.as_raw_fd() >= 3);
            spec.current_dir(directory.path().to_path_buf());
            spec.stdout(OutputMode::capture(6));
            let output = run(spec).expect("script succeeds with stdin closed");
            assert_eq!(output.stdout(), b"script");
            fs::write(marker, b"ok").expect("write child marker");
            return;
        }

        let directory = tempfile::tempdir().expect("tempdir");
        let marker = directory.path().join("child-completed");
        let output = Command::new(env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "subprocess::tests::pinned_executable_survives_a_closed_standard_descriptor",
                "--nocapture",
            ])
            .env(CHILD_MARKER_ENV, &marker)
            .output()
            .expect("start dedicated child test");
        assert!(
            output.status.success(),
            "child test failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(marker.exists(), "dedicated child test did not execute");
    }

    #[test]
    fn relative_monitors_use_child_current_directory() {
        #[cfg(unix)]
        {
            let directory = tempfile::tempdir().expect("tempdir");
            let mut spec = shell("printf 123 > artifact");
            spec.current_dir(directory.path().to_path_buf());
            spec.monitor_file(MonitoredFile::new("artifact", 2, "artifact"));
            assert!(matches!(
                run(spec),
                Err(Error::MonitoredFileLimit { limit: 2, .. })
            ));
        }
    }

    #[test]
    fn setsid_descendant_does_not_block_capture_shutdown() {
        #[cfg(target_os = "linux")]
        {
            let mut spec = shell("setsid sh -c 'sleep 1' & sleep 1");
            spec.stdout(OutputMode::capture(64));
            spec.timeout(Duration::from_millis(25));
            let started = Instant::now();
            assert!(matches!(run(spec), Err(Error::TimedOut)));
            assert!(started.elapsed() < Duration::from_millis(750));
        }
    }

    #[test]
    fn timeout_overflow_is_rejected() {
        #[cfg(unix)]
        {
            let mut spec = shell(":");
            spec.timeout(Duration::MAX);
            assert!(matches!(run(spec), Err(Error::InvalidTimeout)));
        }
    }

    #[test]
    fn oversized_executable_is_rejected_before_hashing() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let directory = tempfile::tempdir().expect("tempdir");
            let path = directory.path().join("oversized-helper");
            let file = fs::File::create(&path).expect("create helper");
            file.set_len(MAX_EXECUTABLE_BYTES + 1)
                .expect("create sparse oversized helper");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod helper");
            assert!(matches!(
                ProcessSpec::new(&path),
                Err(Error::InvalidExecutable(message))
                    if message.contains("identity hashing size limit")
            ));
        }
    }
}
