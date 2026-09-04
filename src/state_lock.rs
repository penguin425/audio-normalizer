//! Process-lifetime locks for atomically replaced state documents.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

/// An exclusive lock held on a persistent sibling of a state document.
///
/// The lock path is intentionally never deleted: removing it would let a
/// second process lock a new inode while the first process still owns the old
/// one. Keeping the lock separate also survives atomic replacement of the
/// state document itself.
#[derive(Debug)]
pub(crate) struct StateFileLock {
    #[allow(dead_code)]
    file: File,
}

impl StateFileLock {
    pub(crate) fn acquire(state_path: &Path, description: &str) -> Result<Self, String> {
        let lock_path = sibling_lock_path(state_path)?;
        if let Some(parent) = lock_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }

        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        #[cfg(windows)]
        options.custom_flags(0x0020_0000);
        let file = options
            .open(&lock_path)
            .map_err(|error| format!("open {description} lock {}: {error}", lock_path.display()))?;
        let metadata = file.metadata().map_err(|error| {
            format!(
                "inspect {description} lock {}: {error}",
                lock_path.display()
            )
        })?;
        #[cfg(windows)]
        if metadata.file_attributes() & 0x0000_0400 != 0 {
            return Err(format!(
                "refuse reparse-point {description} lock {}",
                lock_path.display()
            ));
        }
        if !metadata.is_file() {
            return Err(format!(
                "refuse non-regular {description} lock {}",
                lock_path.display()
            ));
        }
        file.try_lock().map_err(|error| {
            format!(
                "{description} is already open by another process (lock {}): {error}",
                lock_path.display()
            )
        })?;
        Ok(Self { file })
    }
}

pub(crate) fn read_regular_state_file(
    path: &Path,
    description: &str,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(windows)]
    options.custom_flags(0x0020_0000);

    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "open {description} {} without following links: {error}",
                path.display()
            ))
        }
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect opened {description} {}: {error}", path.display()))?;
    #[cfg(windows)]
    if metadata.file_attributes() & 0x0000_0400 != 0 {
        return Err(format!(
            "refuse reparse-point {description} {}",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!(
            "{description} is not a regular file: {}",
            path.display()
        ));
    }
    if metadata.len() > max_bytes {
        return Err(format!(
            "{} exceeds the {max_bytes}-byte {description} limit",
            path.display()
        ));
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {description} {}: {error}", path.display()))?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!(
            "{} exceeds the {max_bytes}-byte {description} limit",
            path.display()
        ));
    }
    Ok(Some(bytes))
}

fn sibling_lock_path(state_path: &Path) -> Result<PathBuf, String> {
    let name = state_path.file_name().ok_or_else(|| {
        format!(
            "state path has no final component for locking: {}",
            state_path.display()
        )
    })?;
    let mut lock_name = name.to_os_string();
    lock_name.push(".lock");
    Ok(state_path.with_file_name(lock_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_and_reusable_after_drop() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("job.json");
        let first = StateFileLock::acquire(&state, "test state").unwrap();
        assert!(StateFileLock::acquire(&state, "test state")
            .unwrap_err()
            .contains("already open"));
        drop(first);
        StateFileLock::acquire(&state, "test state").unwrap();
    }

    #[test]
    fn lock_survives_replacement_of_the_state_inode() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("job.json");
        std::fs::write(&state, b"old").unwrap();
        let first = StateFileLock::acquire(&state, "test state").unwrap();
        let replacement = directory.path().join("replacement");
        std::fs::write(&replacement, b"new").unwrap();
        std::fs::rename(&replacement, &state).unwrap();
        assert!(StateFileLock::acquire(&state, "test state").is_err());
        drop(first);
    }

    #[test]
    fn state_reads_are_bounded_and_use_a_regular_file_handle() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("job.json");
        std::fs::write(&state, b"state").unwrap();
        assert_eq!(
            read_regular_state_file(&state, "test state", 5).unwrap(),
            Some(b"state".to_vec())
        );
        assert!(read_regular_state_file(&state, "test state", 4)
            .unwrap_err()
            .contains("4-byte"));
    }

    #[cfg(unix)]
    #[test]
    fn state_reads_reject_a_final_component_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.json");
        let state = directory.path().join("job.json");
        std::fs::write(&target, b"state").unwrap();
        symlink(&target, &state).unwrap();
        assert!(read_regular_state_file(&state, "test state", 1024).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn final_component_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("job.json");
        let lock = sibling_lock_path(&state).unwrap();
        let target = directory.path().join("target");
        std::fs::write(&target, b"target").unwrap();
        symlink(&target, &lock).unwrap();
        assert!(StateFileLock::acquire(&state, "test state").is_err());
    }
}
