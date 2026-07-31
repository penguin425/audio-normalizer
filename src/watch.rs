//! Durable discovery state for stable-file watch folders.
//!
//! A file becomes eligible only after its size and modification timestamp have
//! remained unchanged for the configured interval. State is committed
//! atomically so a process restart cannot silently lose completed work.

use crate::atomic::AtomicOutput;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const WATCH_FOLDER_SCHEMA_V1: &str =
    "https://penguin425.github.io/audio-normalizer/schema/watch-folder-v1";
const MAX_STATE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_FILES: usize = 100_000;
const MAX_DIRECTORY_ENTRIES: usize = 1_000_000;
const MAX_DIRECTORY_DEPTH: usize = 64;
const MAX_ERROR_BYTES: usize = 4096;
const HASH_BUFFER_BYTES: usize = 1024 * 1024;

/// One stable input returned by [`WatchFolder::scan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchCandidate {
    pub id: String,
    pub input: PathBuf,
    pub relative: PathBuf,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum WatchStatus {
    Observing,
    Processing,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Fingerprint {
    size_bytes: u64,
    modified_unix_ns: u128,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WatchEntry {
    relative: String,
    fingerprint: Fingerprint,
    first_observed_unix_ms: u64,
    status: WatchStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prior_output_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WatchDocument {
    schema: String,
    generator: String,
    input_root: String,
    output_root: String,
    recursive: bool,
    stable_for_ms: u64,
    operation: Value,
    operation_sha256: String,
    entries: Vec<WatchEntry>,
}

/// Atomically persisted state for one input/output watch-folder pair.
#[derive(Debug)]
pub struct WatchFolder {
    state_path: PathBuf,
    input_root: PathBuf,
    output_root: PathBuf,
    document: WatchDocument,
}

impl WatchFolder {
    /// Open existing state or create a new state document.
    pub fn open(
        state_path: impl Into<PathBuf>,
        input_root: impl Into<PathBuf>,
        output_root: impl Into<PathBuf>,
        recursive: bool,
        stable_for: Duration,
        operation: Value,
    ) -> Result<Self, String> {
        let state_path = absolute_path(&state_path.into())?;
        let input_root = std::fs::canonicalize(input_root.into())
            .map_err(|error| format!("canonicalize watch input: {error}"))?;
        if !input_root.is_dir() {
            return Err(format!(
                "watch input is not a directory: {}",
                input_root.display()
            ));
        }
        let output_root = absolute_path(&output_root.into())?;
        std::fs::create_dir_all(&output_root)
            .map_err(|error| format!("create watch output {}: {error}", output_root.display()))?;
        let output_root = std::fs::canonicalize(&output_root)
            .map_err(|error| format!("canonicalize watch output: {error}"))?;
        if input_root == output_root {
            return Err("watch input and output directories must differ".into());
        }
        let stable_for_ms = u64::try_from(stable_for.as_millis())
            .map_err(|_| "watch stable interval is too large".to_string())?;
        if stable_for_ms == 0 {
            return Err("watch stable interval must be greater than zero".into());
        }
        let operation_sha256 = hash_json(&operation)?;
        let input_text = path_text(&input_root)?;
        let output_text = path_text(&output_root)?;

        let document = if state_path.exists() {
            let metadata = std::fs::symlink_metadata(&state_path)
                .map_err(|error| format!("inspect {}: {error}", state_path.display()))?;
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(format!(
                    "watch state is not a regular non-symlink file: {}",
                    state_path.display()
                ));
            }
            if metadata.len() > MAX_STATE_BYTES {
                return Err(format!(
                    "{} exceeds the {MAX_STATE_BYTES}-byte watch state limit",
                    state_path.display()
                ));
            }
            let bytes = std::fs::read(&state_path)
                .map_err(|error| format!("read {}: {error}", state_path.display()))?;
            let document: WatchDocument = serde_json::from_slice(&bytes)
                .map_err(|error| format!("decode {}: {error}", state_path.display()))?;
            validate_document(
                &document,
                &input_text,
                &output_text,
                recursive,
                stable_for_ms,
                &operation,
                &operation_sha256,
            )?;
            document
        } else {
            WatchDocument {
                schema: WATCH_FOLDER_SCHEMA_V1.into(),
                generator: format!("forge-normalizer/{}", env!("CARGO_PKG_VERSION")),
                input_root: input_text,
                output_root: output_text,
                recursive,
                stable_for_ms,
                operation,
                operation_sha256,
                entries: Vec::new(),
            }
        };
        let folder = Self {
            state_path,
            input_root,
            output_root,
            document,
        };
        folder.save()?;
        Ok(folder)
    }

    /// Scan for stable supported audio files in deterministic path order.
    pub fn scan(&mut self) -> Result<Vec<WatchCandidate>, String> {
        self.scan_at(SystemTime::now())
    }

    /// Requeue unchanged failed entries once, preserving any verified output.
    pub fn retry_failed(&mut self) -> Result<usize, String> {
        let mut count = 0;
        for entry in &mut self.document.entries {
            if entry.status == WatchStatus::Failed {
                entry.status = WatchStatus::Observing;
                entry.input_sha256 = None;
                entry.prior_output_sha256 = None;
                entry.error = None;
                count += 1;
            }
        }
        if count != 0 {
            self.save()?;
        }
        Ok(count)
    }

    fn scan_at(&mut self, now: SystemTime) -> Result<Vec<WatchCandidate>, String> {
        let now_ms = unix_millis(now)?;
        let mut discovered = Vec::new();
        let mut visited = 0;
        WatchScanner {
            root: &self.input_root,
            output_root: &self.output_root,
            state_path: &self.state_path,
            recursive: self.document.recursive,
            visited: &mut visited,
            files: &mut discovered,
        }
        .collect(&self.input_root, 0)?;
        if discovered.len() > MAX_FILES {
            return Err(format!("watch folder exceeds the {MAX_FILES}-file limit"));
        }
        discovered.sort();
        let mut seen = BTreeSet::new();
        let mut candidates = Vec::new();

        for input in discovered {
            let relative = input
                .strip_prefix(&self.input_root)
                .map_err(|_| format!("watch path escaped input root: {}", input.display()))?
                .to_owned();
            let id = path_text(&relative)?;
            if !seen.insert(id.clone()) {
                return Err(format!("duplicate watch path: {id}"));
            }
            let fingerprint = fingerprint(&input)?;
            let index = self
                .document
                .entries
                .binary_search_by(|entry| entry.relative.cmp(&id))
                .unwrap_or_else(|index| {
                    self.document.entries.insert(
                        index,
                        WatchEntry {
                            relative: id.clone(),
                            fingerprint: fingerprint.clone(),
                            first_observed_unix_ms: now_ms,
                            status: WatchStatus::Observing,
                            input_sha256: None,
                            output: None,
                            output_sha256: None,
                            prior_output_sha256: None,
                            error: None,
                        },
                    );
                    index
                });
            let entry = &mut self.document.entries[index];
            if entry.fingerprint != fingerprint {
                if matches!(entry.status, WatchStatus::Completed | WatchStatus::Failed)
                    && entry.output_sha256.is_some()
                {
                    if entry
                        .output
                        .as_deref()
                        .map(Path::new)
                        .is_some_and(Path::is_file)
                    {
                        verify_output(entry)?;
                        reset_entry_preserving_output(entry, fingerprint, now_ms);
                    } else {
                        reset_entry(entry, fingerprint, now_ms);
                    }
                } else {
                    reset_entry(entry, fingerprint, now_ms);
                }
                continue;
            }
            match entry.status {
                WatchStatus::Processing => {
                    recover_processing(entry, &input)?;
                }
                WatchStatus::Completed => {
                    verify_completed(entry, &input)?;
                }
                WatchStatus::Observing | WatchStatus::Failed => {}
            }
            if entry.status == WatchStatus::Observing
                && now_ms.saturating_sub(entry.first_observed_unix_ms)
                    >= self.document.stable_for_ms
            {
                candidates.push(WatchCandidate {
                    id,
                    input,
                    relative,
                });
            }
        }
        self.document.entries.retain(|entry| {
            seen.contains(&entry.relative)
                || matches!(
                    entry.status,
                    WatchStatus::Completed | WatchStatus::Processing | WatchStatus::Failed
                )
        });
        self.save()?;
        Ok(candidates)
    }

    /// Persist intent before starting a normalization.
    pub fn mark_processing(
        &mut self,
        id: &str,
        output: impl Into<PathBuf>,
    ) -> Result<PathBuf, String> {
        let input = self.input_root.join(id);
        let output = absolute_path(&output.into())?;
        let output_name = output
            .file_name()
            .ok_or_else(|| format!("watch output has no file name: {}", output.display()))?;
        let parent = output
            .parent()
            .ok_or_else(|| format!("watch output has no parent: {}", output.display()))?;
        let parent = std::fs::canonicalize(parent)
            .map_err(|error| format!("canonicalize watch output parent: {error}"))?;
        if !parent.starts_with(&self.output_root) {
            return Err(format!(
                "watch output escaped output root: {}",
                output.display()
            ));
        }
        let output = parent.join(output_name);
        let entry = self.entry_mut(id)?;
        if entry.status != WatchStatus::Observing {
            return Err(format!("watch entry is not ready for processing: {id}"));
        }
        if fingerprint(&input)? != entry.fingerprint {
            return Err(format!("watch input changed before processing: {id}"));
        }
        let prior_output_sha256 = if output.is_file() {
            if entry.output.as_deref() != output.to_str()
                || entry.output_sha256.as_deref() != Some(hash_file(&output)?.as_str())
            {
                return Err(format!(
                    "refusing to replace an unverified watch output: {}",
                    output.display()
                ));
            }
            entry.output_sha256.take()
        } else {
            None
        };
        entry.input_sha256 = Some(hash_stable_input(&input, &entry.fingerprint)?);
        entry.output = Some(path_text(&output)?);
        entry.output_sha256 = None;
        entry.prior_output_sha256 = prior_output_sha256;
        entry.error = None;
        entry.status = WatchStatus::Processing;
        self.save()?;
        Ok(output)
    }

    /// Verify and atomically checkpoint a successfully committed output.
    pub fn mark_completed(&mut self, id: &str) -> Result<(), String> {
        let input = self.input_root.join(id);
        let entry = self.entry_mut(id)?;
        let actual_input = hash_stable_input(&input, &entry.fingerprint)?;
        if entry.input_sha256.as_deref() != Some(actual_input.as_str()) {
            return Err(format!("watch input changed during processing: {id}"));
        }
        let output = entry
            .output
            .as_deref()
            .map(Path::new)
            .ok_or_else(|| format!("watch entry has no output: {id}"))?;
        entry.output_sha256 = Some(hash_file(output)?);
        entry.prior_output_sha256 = None;
        entry.status = WatchStatus::Completed;
        entry.error = None;
        self.save()
    }

    /// Record a bounded error and suppress retries until the input changes.
    pub fn mark_failed(&mut self, id: &str, error: &str) -> Result<(), String> {
        let entry = self.entry_mut(id)?;
        let mut message = error.to_owned();
        if message.len() > MAX_ERROR_BYTES {
            let mut end = MAX_ERROR_BYTES;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
        }
        entry.status = WatchStatus::Failed;
        entry.error = Some(message);
        entry.prior_output_sha256 = None;
        let output_sha256 = entry
            .output
            .as_deref()
            .map(Path::new)
            .filter(|output| output.is_file())
            .map(hash_file)
            .transpose()?;
        if output_sha256.is_none() {
            entry.output = None;
        }
        entry.output_sha256 = output_sha256;
        self.save()
    }

    fn entry_mut(&mut self, id: &str) -> Result<&mut WatchEntry, String> {
        self.document
            .entries
            .binary_search_by(|entry| entry.relative.as_str().cmp(id))
            .ok()
            .map(|index| &mut self.document.entries[index])
            .ok_or_else(|| format!("unknown watch entry: {id}"))
    }

    fn save(&self) -> Result<(), String> {
        if let Some(parent) = self
            .state_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        let mut bytes = serde_json::to_vec_pretty(&self.document)
            .map_err(|error| format!("encode watch state: {error}"))?;
        bytes.push(b'\n');
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(format!(
                "watch state exceeds the {MAX_STATE_BYTES}-byte limit"
            ));
        }
        let mut output = AtomicOutput::new(&self.state_path)?;
        output.write_all(&bytes)?;
        output.commit()
    }
}

fn reset_entry(entry: &mut WatchEntry, fingerprint: Fingerprint, now_ms: u64) {
    entry.fingerprint = fingerprint;
    entry.first_observed_unix_ms = now_ms;
    entry.status = WatchStatus::Observing;
    entry.input_sha256 = None;
    entry.output = None;
    entry.output_sha256 = None;
    entry.prior_output_sha256 = None;
    entry.error = None;
}

fn reset_entry_preserving_output(entry: &mut WatchEntry, fingerprint: Fingerprint, now_ms: u64) {
    entry.fingerprint = fingerprint;
    entry.first_observed_unix_ms = now_ms;
    entry.status = WatchStatus::Observing;
    entry.input_sha256 = None;
    entry.prior_output_sha256 = None;
    entry.error = None;
}

fn recover_processing(entry: &mut WatchEntry, input: &Path) -> Result<(), String> {
    let actual_input = hash_stable_input(input, &entry.fingerprint)?;
    if entry.input_sha256.as_deref() != Some(actual_input.as_str()) {
        return Err(format!(
            "watch input changed while recovering: {}",
            input.display()
        ));
    }
    let output = entry
        .output
        .as_deref()
        .map(Path::new)
        .ok_or_else(|| "processing watch entry has no output".to_string())?;
    if output.is_file() {
        let actual_output = hash_file(output)?;
        if entry.prior_output_sha256.as_deref() == Some(actual_output.as_str()) {
            entry.input_sha256 = None;
            entry.output_sha256 = Some(actual_output);
            entry.prior_output_sha256 = None;
            entry.status = WatchStatus::Observing;
        } else {
            entry.output_sha256 = Some(actual_output);
            entry.prior_output_sha256 = None;
            entry.status = WatchStatus::Completed;
        }
    } else {
        let fingerprint = entry.fingerprint.clone();
        let observed = entry.first_observed_unix_ms;
        reset_entry(entry, fingerprint, observed);
    }
    Ok(())
}

fn verify_completed(entry: &mut WatchEntry, input: &Path) -> Result<(), String> {
    let actual_input = hash_stable_input(input, &entry.fingerprint)?;
    if entry.input_sha256.as_deref() != Some(actual_input.as_str()) {
        let next_fingerprint = fingerprint(input)?;
        let now_ms = unix_millis(SystemTime::now())?;
        if entry
            .output
            .as_deref()
            .map(Path::new)
            .is_some_and(Path::is_file)
        {
            verify_output(entry)?;
            reset_entry_preserving_output(entry, next_fingerprint, now_ms);
        } else {
            reset_entry(entry, next_fingerprint, now_ms);
        }
        return Ok(());
    }
    let output = entry
        .output
        .as_deref()
        .map(Path::new)
        .ok_or_else(|| "completed watch entry has no output".to_string())?;
    if !output.is_file() {
        let fingerprint = entry.fingerprint.clone();
        let observed = entry.first_observed_unix_ms;
        reset_entry(entry, fingerprint, observed);
        return Ok(());
    }
    verify_output(entry)
}

fn verify_output(entry: &WatchEntry) -> Result<(), String> {
    let output = entry
        .output
        .as_deref()
        .map(Path::new)
        .ok_or_else(|| "completed watch entry has no output".to_string())?;
    let actual = hash_file(output)?;
    if entry.output_sha256.as_deref() != Some(actual.as_str()) {
        return Err(format!(
            "completed watch output changed since checkpoint: {}",
            output.display()
        ));
    }
    Ok(())
}

struct WatchScanner<'a> {
    root: &'a Path,
    output_root: &'a Path,
    state_path: &'a Path,
    recursive: bool,
    visited: &'a mut usize,
    files: &'a mut Vec<PathBuf>,
}

impl WatchScanner<'_> {
    fn collect(&mut self, directory: &Path, depth: usize) -> Result<(), String> {
        if depth > MAX_DIRECTORY_DEPTH {
            return Err(format!(
                "watch folder exceeds the {MAX_DIRECTORY_DEPTH}-directory-depth limit"
            ));
        }
        let mut entries = std::fs::read_dir(directory)
            .map_err(|error| format!("read {}: {error}", directory.display()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("read {}: {error}", directory.display()))?;
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            *self.visited = self
                .visited
                .checked_add(1)
                .ok_or_else(|| "watch directory-entry count overflow".to_string())?;
            if *self.visited > MAX_DIRECTORY_ENTRIES {
                return Err(format!(
                    "watch folder exceeds the {MAX_DIRECTORY_ENTRIES}-directory-entry limit"
                ));
            }
            let file_type = entry
                .file_type()
                .map_err(|error| format!("inspect {}: {error}", entry.path().display()))?;
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if path == self.state_path {
                continue;
            }
            if file_type.is_dir() {
                if self.recursive && path != self.output_root && path.starts_with(self.root) {
                    self.collect(&path, depth + 1)?;
                }
            } else if file_type.is_file() && supported_audio(&path) {
                self.files.push(path);
            }
        }
        Ok(())
    }
}

fn supported_audio(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some(
            "wav"
                | "wave"
                | "bwf"
                | "bw64"
                | "rf64"
                | "dsf"
                | "dff"
                | "mp3"
                | "flac"
                | "aac"
                | "m4a"
                | "mp4"
                | "ogg"
                | "opus"
        )
    )
}

fn fingerprint(path: &Path) -> Result<Fingerprint, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(format!(
            "watch input is not a regular non-symlink file: {}",
            path.display()
        ));
    }
    let modified = metadata
        .modified()
        .map_err(|error| format!("read modification time {}: {error}", path.display()))?
        .duration_since(UNIX_EPOCH)
        .map_err(|_| format!("modification time predates Unix epoch: {}", path.display()))?;
    Ok(Fingerprint {
        size_bytes: metadata.len(),
        modified_unix_ns: modified.as_nanos(),
    })
}

fn validate_document(
    document: &WatchDocument,
    input_root: &str,
    output_root: &str,
    recursive: bool,
    stable_for_ms: u64,
    operation: &Value,
    operation_sha256: &str,
) -> Result<(), String> {
    if document.schema != WATCH_FOLDER_SCHEMA_V1 {
        return Err(format!(
            "unsupported watch state schema: {}",
            document.schema
        ));
    }
    if document.generator != format!("forge-normalizer/{}", env!("CARGO_PKG_VERSION")) {
        return Err(format!(
            "watch state generator does not match this Forge version: {}",
            document.generator
        ));
    }
    if document.input_root != input_root
        || document.output_root != output_root
        || document.recursive != recursive
        || document.stable_for_ms != stable_for_ms
        || document.operation != *operation
        || document.operation_sha256 != operation_sha256
    {
        return Err("watch state does not match this invocation".into());
    }
    if document.entries.len() > MAX_FILES {
        return Err(format!("watch state exceeds the {MAX_FILES}-entry limit"));
    }
    if hash_json(&document.operation)? != document.operation_sha256 {
        return Err("watch state operation hash is invalid".into());
    }
    let mut previous: Option<&str> = None;
    for entry in &document.entries {
        if previous.is_some_and(|value| value >= entry.relative.as_str()) {
            return Err("watch state entries must be unique and sorted".into());
        }
        previous = Some(&entry.relative);
        validate_entry(entry)?;
    }
    Ok(())
}

fn validate_entry(entry: &WatchEntry) -> Result<(), String> {
    if entry.relative.is_empty()
        || Path::new(&entry.relative).is_absolute()
        || Path::new(&entry.relative)
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!("invalid watch relative path: {}", entry.relative));
    }
    let has_processing = entry.input_sha256.is_some() && entry.output.is_some();
    match entry.status {
        WatchStatus::Observing => {
            if entry.input_sha256.is_some()
                || entry.prior_output_sha256.is_some()
                || entry.error.is_some()
                || entry.output.is_some() != entry.output_sha256.is_some()
            {
                return Err("observing watch entry contains processing fields".into());
            }
        }
        WatchStatus::Processing => {
            if !has_processing || entry.output_sha256.is_some() || entry.error.is_some() {
                return Err("invalid processing watch entry".into());
            }
        }
        WatchStatus::Completed => {
            if !has_processing
                || entry.output_sha256.is_none()
                || entry.prior_output_sha256.is_some()
                || entry.error.is_some()
            {
                return Err("invalid completed watch entry".into());
            }
        }
        WatchStatus::Failed => {
            if entry.error.is_none()
                || entry.prior_output_sha256.is_some()
                || entry.output.is_some() != entry.output_sha256.is_some()
            {
                return Err("invalid failed watch entry".into());
            }
        }
    }
    for hash in [
        &entry.input_sha256,
        &entry.output_sha256,
        &entry.prior_output_sha256,
    ]
    .into_iter()
    .flatten()
    {
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("watch state contains an invalid SHA-256".into());
        }
    }
    Ok(())
}

fn unix_millis(time: SystemTime) -> Result<u64, String> {
    u64::try_from(
        time.duration_since(UNIX_EPOCH)
            .map_err(|_| "system time predates Unix epoch".to_string())?
            .as_millis(),
    )
    .map_err(|_| "system time is too large".to_string())
}

fn hash_json(value: &Value) -> Result<String, String> {
    let bytes = serde_json::to_vec(value).map_err(|error| format!("encode operation: {error}"))?;
    Ok(hex_digest(Sha256::digest(bytes)))
}

fn hash_file(path: &Path) -> Result<String, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(format!(
            "refusing to hash a non-regular or symlink file: {}",
            path.display()
        ));
    }
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_BUFFER_BYTES];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex_digest(hasher.finalize()))
}

fn hash_stable_input(path: &Path, expected: &Fingerprint) -> Result<String, String> {
    let before = fingerprint(path)?;
    if &before != expected {
        return Err(format!(
            "watch input changed before hashing: {}",
            path.display()
        ));
    }
    let hash = hash_file(path)?;
    if fingerprint(path)? != before {
        return Err(format!(
            "watch input changed while hashing: {}",
            path.display()
        ));
    }
    Ok(hash)
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    let mut text = String::with_capacity(64);
    for byte in bytes.as_ref() {
        write!(&mut text, "{byte:02x}").expect("writing to String cannot fail");
    }
    text
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    std::path::absolute(path).map_err(|error| format!("resolve {}: {error}", path.display()))
}

fn path_text(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assert_schema_valid(path: &Path) {
        let schema: Value =
            serde_json::from_str(include_str!("../schema/watch-folder-v1.schema.json")).unwrap();
        let document: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let errors = validator
            .iter_errors(&document)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        assert!(errors.is_empty(), "{errors:#?}\n{document:#}");
    }

    fn folder(
        root: &Path,
        stable_for: Duration,
        now: SystemTime,
    ) -> (WatchFolder, PathBuf, PathBuf) {
        let input = root.join("input");
        let output = root.join("output");
        let state = root.join("watch.json");
        std::fs::create_dir(&input).unwrap();
        let mut folder = WatchFolder::open(
            &state,
            &input,
            &output,
            true,
            stable_for,
            json!({"target": -16}),
        )
        .unwrap();
        folder.scan_at(now).unwrap();
        (folder, input, output)
    }

    #[test]
    fn requires_an_unchanged_observation_window() {
        let directory = tempfile::tempdir().unwrap();
        let start = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (mut folder, input, _) = folder(directory.path(), Duration::from_secs(5), start);
        std::fs::write(input.join("tone.wav"), b"first").unwrap();
        assert!(folder.scan_at(start).unwrap().is_empty());
        assert!(folder
            .scan_at(start + Duration::from_secs(4))
            .unwrap()
            .is_empty());
        let ready = folder.scan_at(start + Duration::from_secs(5)).unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "tone.wav");
    }

    #[test]
    fn changed_input_restarts_the_stability_window() {
        let directory = tempfile::tempdir().unwrap();
        let start = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (mut folder, input, _) = folder(directory.path(), Duration::from_secs(5), start);
        let file = input.join("tone.wav");
        std::fs::write(&file, b"first").unwrap();
        folder.scan_at(start).unwrap();
        std::fs::write(&file, b"changed bytes").unwrap();
        assert!(folder
            .scan_at(start + Duration::from_secs(5))
            .unwrap()
            .is_empty());
        assert_eq!(
            folder
                .scan_at(start + Duration::from_secs(10))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn completed_outputs_are_verified_and_missing_outputs_are_requeued() {
        let directory = tempfile::tempdir().unwrap();
        let start = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (mut folder, input, output) = folder(directory.path(), Duration::from_secs(1), start);
        std::fs::write(input.join("tone.wav"), b"audio").unwrap();
        folder.scan_at(start).unwrap();
        let candidate = folder
            .scan_at(start + Duration::from_secs(1))
            .unwrap()
            .remove(0);
        let rendered = output.join("tone_normalized.wav");
        folder.mark_processing(&candidate.id, &rendered).unwrap();
        std::fs::write(&rendered, b"normalized").unwrap();
        folder.mark_completed(&candidate.id).unwrap();
        assert_schema_valid(&directory.path().join("watch.json"));
        assert!(folder
            .scan_at(start + Duration::from_secs(2))
            .unwrap()
            .is_empty());
        std::fs::remove_file(&rendered).unwrap();
        assert_eq!(
            folder
                .scan_at(start + Duration::from_secs(3))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn changed_completed_output_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let start = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (mut folder, input, output) = folder(directory.path(), Duration::from_secs(1), start);
        std::fs::write(input.join("tone.wav"), b"audio").unwrap();
        folder.scan_at(start).unwrap();
        let candidate = folder
            .scan_at(start + Duration::from_secs(1))
            .unwrap()
            .remove(0);
        let rendered = output.join("tone_normalized.wav");
        folder.mark_processing(&candidate.id, &rendered).unwrap();
        std::fs::write(&rendered, b"normalized").unwrap();
        folder.mark_completed(&candidate.id).unwrap();
        std::fs::write(&rendered, b"tampered").unwrap();
        assert!(folder
            .scan_at(start + Duration::from_secs(2))
            .unwrap_err()
            .contains("changed since checkpoint"));
    }

    #[test]
    fn changed_input_can_replace_only_its_verified_prior_output() {
        let directory = tempfile::tempdir().unwrap();
        let start = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (mut folder, input, output) = folder(directory.path(), Duration::from_secs(1), start);
        let source = input.join("tone.wav");
        std::fs::write(&source, b"first audio").unwrap();
        folder.scan_at(start).unwrap();
        let candidate = folder
            .scan_at(start + Duration::from_secs(1))
            .unwrap()
            .remove(0);
        let rendered = output.join("tone_normalized.wav");
        folder.mark_processing(&candidate.id, &rendered).unwrap();
        std::fs::write(&rendered, b"first output").unwrap();
        folder.mark_completed(&candidate.id).unwrap();

        std::fs::write(&source, b"changed audio bytes").unwrap();
        assert!(folder
            .scan_at(start + Duration::from_secs(2))
            .unwrap()
            .is_empty());
        let changed = folder
            .scan_at(start + Duration::from_secs(3))
            .unwrap()
            .remove(0);
        folder.mark_processing(&changed.id, &rendered).unwrap();
        std::fs::write(&rendered, b"second output").unwrap();
        folder.mark_completed(&changed.id).unwrap();
        assert!(folder
            .scan_at(start + Duration::from_secs(4))
            .unwrap()
            .is_empty());
        assert_schema_valid(&directory.path().join("watch.json"));
    }

    #[test]
    fn restart_recovers_a_committed_processing_output() {
        let directory = tempfile::tempdir().unwrap();
        let start = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (mut folder, input, output) = folder(directory.path(), Duration::from_secs(1), start);
        std::fs::write(input.join("tone.wav"), b"audio").unwrap();
        folder.scan_at(start).unwrap();
        let candidate = folder
            .scan_at(start + Duration::from_secs(1))
            .unwrap()
            .remove(0);
        let rendered = output.join("tone_normalized.wav");
        folder.mark_processing(&candidate.id, &rendered).unwrap();
        std::fs::write(&rendered, b"normalized").unwrap();
        drop(folder);

        let mut reopened = WatchFolder::open(
            directory.path().join("watch.json"),
            &input,
            &output,
            true,
            Duration::from_secs(1),
            json!({"target": -16}),
        )
        .unwrap();
        assert!(reopened
            .scan_at(start + Duration::from_secs(2))
            .unwrap()
            .is_empty());
        assert_schema_valid(&directory.path().join("watch.json"));
    }

    #[test]
    fn restart_requeues_processing_when_no_output_was_committed() {
        let directory = tempfile::tempdir().unwrap();
        let start = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (mut folder, input, output) = folder(directory.path(), Duration::from_secs(1), start);
        std::fs::write(input.join("tone.wav"), b"audio").unwrap();
        folder.scan_at(start).unwrap();
        let candidate = folder
            .scan_at(start + Duration::from_secs(1))
            .unwrap()
            .remove(0);
        folder
            .mark_processing(&candidate.id, output.join("tone.wav"))
            .unwrap();
        drop(folder);

        let mut reopened = WatchFolder::open(
            directory.path().join("watch.json"),
            &input,
            &output,
            true,
            Duration::from_secs(1),
            json!({"target": -16}),
        )
        .unwrap();
        assert_eq!(
            reopened
                .scan_at(start + Duration::from_secs(2))
                .unwrap()
                .len(),
            1
        );
        assert_schema_valid(&directory.path().join("watch.json"));
    }

    #[test]
    fn failed_entries_require_an_explicit_retry_or_input_change() {
        let directory = tempfile::tempdir().unwrap();
        let start = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (mut folder, input, output) = folder(directory.path(), Duration::from_secs(1), start);
        std::fs::write(input.join("tone.wav"), b"audio").unwrap();
        folder.scan_at(start).unwrap();
        let candidate = folder
            .scan_at(start + Duration::from_secs(1))
            .unwrap()
            .remove(0);
        folder
            .mark_processing(&candidate.id, output.join("tone.wav"))
            .unwrap();
        folder
            .mark_failed(&candidate.id, &"é".repeat(3000))
            .unwrap();
        let state_value: Value =
            serde_json::from_slice(&std::fs::read(directory.path().join("watch.json")).unwrap())
                .unwrap();
        assert!(state_value["entries"][0]["error"].as_str().unwrap().len() <= MAX_ERROR_BYTES);
        assert!(folder
            .scan_at(start + Duration::from_secs(2))
            .unwrap()
            .is_empty());
        assert_eq!(folder.retry_failed().unwrap(), 1);
        assert_eq!(
            folder
                .scan_at(start + Duration::from_secs(2))
                .unwrap()
                .len(),
            1
        );
        assert_schema_valid(&directory.path().join("watch.json"));
    }

    #[test]
    fn state_is_bound_to_operation_and_roots() {
        let directory = tempfile::tempdir().unwrap();
        let start = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (folder, input, output) = folder(directory.path(), Duration::from_secs(1), start);
        drop(folder);
        let error = WatchFolder::open(
            directory.path().join("watch.json"),
            input,
            output,
            true,
            Duration::from_secs(1),
            json!({"target": -23}),
        )
        .unwrap_err();
        assert!(error.contains("does not match"));
    }

    #[test]
    fn refuses_an_unknown_existing_output() {
        let directory = tempfile::tempdir().unwrap();
        let start = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (mut folder, input, output) = folder(directory.path(), Duration::from_secs(1), start);
        std::fs::write(input.join("tone.wav"), b"audio").unwrap();
        folder.scan_at(start).unwrap();
        let candidate = folder
            .scan_at(start + Duration::from_secs(1))
            .unwrap()
            .remove(0);
        let rendered = output.join("tone.wav");
        std::fs::write(&rendered, b"unrelated").unwrap();
        assert!(folder
            .mark_processing(&candidate.id, rendered)
            .unwrap_err()
            .contains("unverified"));
    }

    #[test]
    fn directory_depth_is_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let start = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (mut folder, input, _) = folder(directory.path(), Duration::from_secs(1), start);
        let mut nested = input;
        for index in 0..=MAX_DIRECTORY_DEPTH {
            nested = nested.join(format!("d{index}"));
            std::fs::create_dir(&nested).unwrap();
        }
        assert!(folder
            .scan_at(start + Duration::from_secs(1))
            .unwrap_err()
            .contains("directory-depth limit"));
    }

    #[test]
    fn schema_rejects_unknown_status_and_incomplete_completion() {
        let schema: Value =
            serde_json::from_str(include_str!("../schema/watch-folder-v1.schema.json")).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let base = json!({
            "schema": WATCH_FOLDER_SCHEMA_V1,
            "generator": "forge-normalizer/1.2.3",
            "input_root": "/input",
            "output_root": "/output",
            "recursive": false,
            "stable_for_ms": 5000,
            "operation": {},
            "operation_sha256": "0".repeat(64),
            "entries": [{
                "relative": "tone.wav",
                "fingerprint": {"size_bytes": 1, "modified_unix_ns": 1},
                "first_observed_unix_ms": 1,
                "status": "completed",
                "input_sha256": "1".repeat(64),
                "output": "/output/tone.wav"
            }]
        });
        assert!(!validator.is_valid(&base));
        let mut unknown = base;
        unknown["entries"][0]["status"] = json!("unknown");
        assert!(!validator.is_valid(&unknown));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let start = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (mut folder, input, _) = folder(directory.path(), Duration::from_secs(1), start);
        let outside = directory.path().join("outside.wav");
        std::fs::write(&outside, b"audio").unwrap();
        symlink(&outside, input.join("link.wav")).unwrap();
        assert!(folder
            .scan_at(start + Duration::from_secs(2))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn state_file_is_excluded_even_with_an_audio_extension() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input");
        let output = directory.path().join("output");
        std::fs::create_dir(&input).unwrap();
        let mut folder = WatchFolder::open(
            input.join("state.wav"),
            &input,
            output,
            false,
            Duration::from_secs(1),
            json!({}),
        )
        .unwrap();
        assert!(folder.scan().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_replacement_after_scan_is_rejected_before_hashing() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let start = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (mut folder, input, output) = folder(directory.path(), Duration::from_secs(1), start);
        let source = input.join("tone.wav");
        std::fs::write(&source, b"audio").unwrap();
        folder.scan_at(start).unwrap();
        let candidate = folder
            .scan_at(start + Duration::from_secs(1))
            .unwrap()
            .remove(0);
        let outside = directory.path().join("outside.wav");
        std::fs::write(&outside, b"audio").unwrap();
        std::fs::remove_file(&source).unwrap();
        symlink(&outside, &source).unwrap();
        assert!(folder
            .mark_processing(&candidate.id, output.join("tone.wav"))
            .unwrap_err()
            .contains("non-symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_output_parent_cannot_escape_the_output_root() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let start = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (mut folder, input, output) = folder(directory.path(), Duration::from_secs(1), start);
        std::fs::write(input.join("tone.wav"), b"audio").unwrap();
        folder.scan_at(start).unwrap();
        let candidate = folder
            .scan_at(start + Duration::from_secs(1))
            .unwrap()
            .remove(0);
        let outside = directory.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, output.join("escaped")).unwrap();
        assert!(folder
            .mark_processing(&candidate.id, output.join("escaped/tone.wav"))
            .unwrap_err()
            .contains("escaped output root"));
    }
}
