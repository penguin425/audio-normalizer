//! Whole-invocation validation for filesystem outputs.

use crate::stable_input::{path_identity_if_exists, StableFileIdentity};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

/// A file that an invocation must never replace or alias.
#[derive(Clone, Debug)]
pub struct ProtectedPath {
    label: String,
    path: PathBuf,
}

impl ProtectedPath {
    /// Describe a protected input or control file.
    pub fn new(label: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            label: label.into(),
            path: path.into(),
        }
    }
}

/// One file an invocation intends to create or replace.
#[derive(Clone, Debug)]
pub struct PlannedOutput {
    label: String,
    path: PathBuf,
    overwrite: bool,
}

impl PlannedOutput {
    /// Describe an output and whether an existing regular file may be replaced.
    pub fn new(label: impl Into<String>, path: impl Into<PathBuf>, overwrite: bool) -> Self {
        Self {
            label: label.into(),
            path: path.into(),
            overwrite,
        }
    }
}

/// A validated set of protected paths and filesystem outputs.
///
/// Construction resolves existing ancestors without creating directories and
/// rejects duplicate routes, hard-link aliases, conservative Windows/macOS
/// case-folding aliases, output-to-input aliases, output links/reparse points,
/// non-regular destinations, and unrequested replacement.
#[derive(Clone, Debug)]
pub struct OutputPlan {
    protected: Vec<ProtectedPath>,
    outputs: Vec<PlannedOutput>,
}

impl OutputPlan {
    /// Validate and retain a complete invocation path plan.
    pub fn new(protected: Vec<ProtectedPath>, outputs: Vec<PlannedOutput>) -> Result<Self, String> {
        let plan = Self { protected, outputs };
        plan.validate()?;
        Ok(plan)
    }

    /// Revalidate the path relationships and overwrite policy against the
    /// current filesystem state without creating or modifying any path.
    pub fn validate(&self) -> Result<(), String> {
        let mut protected_routes: HashMap<String, (&str, &Path)> = HashMap::new();
        let mut protected_identities: HashMap<StableFileIdentity, (&str, &Path)> = HashMap::new();
        for protected in &self.protected {
            let inspected = inspect_path(&protected.path, false, &protected.label)?;
            protected_routes
                .entry(inspected.route_key)
                .or_insert((&protected.label, &protected.path));
            if let Some(identity) = inspected.identity {
                protected_identities
                    .entry(identity)
                    .or_insert((&protected.label, &protected.path));
            }
        }

        let mut output_routes: HashMap<String, (&str, &Path)> = HashMap::new();
        let mut output_identities: HashMap<StableFileIdentity, (&str, &Path)> = HashMap::new();
        for output in &self.outputs {
            let inspected = inspect_path(&output.path, true, &output.label)?;
            if inspected.exists && !output.overwrite {
                return Err(format!(
                    "{} already exists: {} (use --overwrite)",
                    output.label,
                    output.path.display()
                ));
            }
            if let Some((label, path)) = protected_routes.get(&inspected.route_key) {
                return Err(format!(
                    "{} {} aliases protected {} {}",
                    output.label,
                    output.path.display(),
                    label,
                    path.display()
                ));
            }
            if let Some(identity) = inspected.identity.as_ref() {
                if let Some((label, path)) = protected_identities.get(identity) {
                    return Err(format!(
                        "{} {} hard-links protected {} {}",
                        output.label,
                        output.path.display(),
                        label,
                        path.display()
                    ));
                }
            }
            if let Some((label, path)) =
                output_routes.insert(inspected.route_key, (&output.label, output.path.as_path()))
            {
                return Err(format!(
                    "{} {} collides with {} {}",
                    output.label,
                    output.path.display(),
                    label,
                    path.display()
                ));
            }
            if let Some(identity) = inspected.identity {
                if let Some((label, path)) =
                    output_identities.insert(identity, (&output.label, output.path.as_path()))
                {
                    return Err(format!(
                        "{} {} hard-links {} {}",
                        output.label,
                        output.path.display(),
                        label,
                        path.display()
                    ));
                }
            }
        }
        Ok(())
    }

    /// Number of filesystem outputs in the plan.
    pub fn output_count(&self) -> usize {
        self.outputs.len()
    }
}

struct InspectedPath {
    route_key: String,
    identity: Option<StableFileIdentity>,
    exists: bool,
}

fn inspect_path(path: &Path, output: bool, label: &str) -> Result<InspectedPath, String> {
    if path.as_os_str().is_empty() || path.file_name().is_none() {
        return Err(format!("{label} must name a file: {}", path.display()));
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("inspect {label} {}: {error}", path.display())),
    };
    if output {
        if metadata.as_ref().is_some_and(metadata_is_link) {
            return Err(format!(
                "{label} must not be a symbolic link or reparse point: {}",
                path.display()
            ));
        }
        if metadata
            .as_ref()
            .is_some_and(|metadata| !metadata.is_file())
        {
            return Err(format!("{label} is not a regular file: {}", path.display()));
        }
    }
    let resolved = resolve_path(path, 0)?;
    let identity = if metadata.as_ref().is_some_and(|metadata| metadata.is_file()) {
        path_identity_if_exists(path)
            .map_err(|error| format!("identify {label} {}: {error}", path.display()))?
    } else {
        None
    };
    Ok(InspectedPath {
        route_key: route_key(&resolved),
        identity,
        exists: metadata.is_some(),
    })
}

fn resolve_path(path: &Path, symlink_depth: usize) -> Result<PathBuf, String> {
    const MAX_SYMLINK_DEPTH: usize = 40;
    if symlink_depth >= MAX_SYMLINK_DEPTH {
        return Err(format!(
            "too many symbolic links while resolving {}",
            path.display()
        ));
    }
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("resolve {}: {error}", path.display()))?
            .join(path)
    };
    let lexical = lexical_normalize(&absolute)?;
    match std::fs::canonicalize(&lexical) {
        Ok(resolved) => return Ok(resolved),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(format!("resolve {}: {error}", path.display()))
        }
        Err(_) => {}
    }

    if let Ok(metadata) = std::fs::symlink_metadata(&lexical) {
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&lexical)
                .map_err(|error| format!("read link {}: {error}", lexical.display()))?;
            let target = if target.is_absolute() {
                target
            } else {
                lexical
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(target)
            };
            return resolve_path(&target, symlink_depth + 1);
        }
    }

    let mut ancestor = lexical.as_path();
    let mut suffix = Vec::new();
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = ancestor
                    .file_name()
                    .ok_or_else(|| format!("cannot resolve path {}", path.display()))?;
                suffix.push(name.to_owned());
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| format!("cannot resolve path {}", path.display()))?;
            }
            Err(error) => {
                return Err(format!(
                    "inspect ancestor {} while resolving {}: {error}",
                    ancestor.display(),
                    path.display()
                ));
            }
        }
    }
    let mut resolved = std::fs::canonicalize(ancestor)
        .map_err(|error| format!("resolve {}: {error}", path.display()))?;
    for name in suffix.iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

fn lexical_normalize(path: &Path) -> Result<PathBuf, String> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(format!(
                        "path escapes its filesystem root: {}",
                        path.display()
                    ));
                }
            }
        }
    }
    Ok(normalized)
}

#[cfg(windows)]
fn metadata_is_link(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    metadata.file_type().is_symlink() || metadata.file_attributes() & 0x0000_0400 != 0
}

#[cfg(not(windows))]
fn metadata_is_link(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn route_key(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .split('\\')
        .map(|component| component.trim_end_matches([' ', '.']).to_lowercase())
        .collect::<Vec<_>>()
        .join("\\")
}

#[cfg(target_os = "macos")]
fn route_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn route_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicate_and_protected_routes() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.wav");
        let output = directory.path().join("output.wav");
        std::fs::write(&input, b"input").unwrap();

        let error = OutputPlan::new(
            vec![ProtectedPath::new("input", &input)],
            vec![PlannedOutput::new("audio output", &input, true)],
        )
        .unwrap_err();
        assert!(error.contains("protected"), "{error}");

        let error = OutputPlan::new(
            Vec::new(),
            vec![
                PlannedOutput::new("first", &output, true),
                PlannedOutput::new("second", directory.path().join("./output.wav"), true),
            ],
        )
        .unwrap_err();
        assert!(error.contains("collides"), "{error}");
    }

    #[test]
    fn rejects_existing_output_without_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("report.json");
        std::fs::write(&output, b"existing").unwrap();
        let error = OutputPlan::new(
            Vec::new(),
            vec![PlannedOutput::new("report", &output, false)],
        )
        .unwrap_err();
        assert!(error.contains("already exists"), "{error}");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn rejects_hard_link_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.wav");
        let output = directory.path().join("output.wav");
        std::fs::write(&input, b"input").unwrap();
        std::fs::hard_link(&input, &output).unwrap();
        let error = OutputPlan::new(
            vec![ProtectedPath::new("input", &input)],
            vec![PlannedOutput::new("output", &output, true)],
        )
        .unwrap_err();
        assert!(error.contains("hard-links protected"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_output_symlinks_even_when_overwrite_is_enabled() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        let output = directory.path().join("output");
        std::fs::write(&target, b"target").unwrap();
        symlink(&target, &output).unwrap();
        assert!(OutputPlan::new(
            Vec::new(),
            vec![PlannedOutput::new("output", &output, true)]
        )
        .is_err());
    }

    #[test]
    fn validation_does_not_create_missing_parents() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("missing/child");
        let output = parent.join("output.wav");
        let plan = OutputPlan::new(
            Vec::new(),
            vec![PlannedOutput::new("output", output, false)],
        )
        .unwrap();
        assert_eq!(plan.output_count(), 1);
        assert!(!parent.exists());
    }
}
