//! Project manifest (`lullaby.json`) loading and resolution.
//!
//! A Lullaby *project* is a directory containing a `lullaby.json` manifest that
//! names the project, an optional executable entry file, one or more source
//! directories, and zero or more local path dependencies (other project roots
//! that each contain their own `lullaby.json`). This module parses and validates
//! the manifest with `serde_json`, then resolves the full set of source search
//! directories a build should see: the project's own `src` directories followed
//! by the `src` directories of every transitively resolved dependency.
//!
//! Remote/registry dependency *fetching* is deferred; dependencies are local
//! paths only. All manifest/resolution failures are reported as `L0343` loader
//! diagnostics.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use lullaby_diagnostics::{DiagnosticPhase, DiagnosticReport};
use serde::Deserialize;

/// The canonical manifest file name at a project root.
pub const MANIFEST_FILE_NAME: &str = "lullaby.json";

/// A parsed `lullaby.json` manifest.
///
/// All paths are stored exactly as written (relative to the manifest directory);
/// resolution against the manifest directory happens in [`ResolvedProject`].
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectManifest {
    /// The project name.
    pub name: String,
    /// The project's own version: a semver-shaped `MAJOR.MINOR.PATCH` string with
    /// an optional `-<prerelease>` suffix (e.g. `0.1.0`, `1.2.0`,
    /// `1.0.0-preview`). Optional and backward-compatible — manifests written
    /// before this field existed (no `version`) load unchanged. When present it
    /// must be well-formed or the manifest is rejected with `L0343`. This mirrors
    /// the toolchain's own `MAJOR.PATCH-STATUS` display scheme mapped onto semver
    /// (see `documents/versioning.md`) and reserves the field so a future package
    /// registry can add version *constraints* without breaking existing manifests.
    #[serde(default)]
    pub version: Option<String>,
    /// The executable entry `.lby` file, relative to the manifest directory.
    /// Optional: library projects have no entry.
    #[serde(default)]
    pub entry: Option<String>,
    /// Source directories relative to the manifest directory. Defaults to
    /// `["."]` when omitted.
    #[serde(default = "default_src")]
    pub src: Vec<String>,
    /// Local path dependencies: dependency name -> path to another project root
    /// (a directory containing its own `lullaby.json`), relative to the manifest
    /// directory. Defaults to empty.
    #[serde(default)]
    pub dependencies: std::collections::BTreeMap<String, String>,
}

fn default_src() -> Vec<String> {
    vec![".".to_string()]
}

/// A fully resolved project: the manifest, its root directory, the absolute
/// entry path (if any), and the ordered list of source search directories that a
/// build should consult (this project's `src` dirs first, then every dependency's
/// `src` dirs, transitively, de-duplicated).
#[derive(Debug, Clone)]
pub struct ResolvedProject {
    pub manifest: ProjectManifest,
    pub entry: Option<PathBuf>,
    pub search_dirs: Vec<PathBuf>,
}

/// Build an `L0343` loader diagnostic for a manifest/resolution failure.
fn manifest_error(message: String, path: &Path) -> Box<DiagnosticReport> {
    Box::new(
        DiagnosticReport::new("L0343", DiagnosticPhase::Loader, message)
            .with_source_path(path.display().to_string()),
    )
}

/// Given a CLI path argument that is either a project directory or a
/// `lullaby.json` file, return the directory that contains the manifest and the
/// manifest file path itself. Returns `None` when the argument is neither (the
/// caller then treats it as a single `.lby` file, preserving legacy behavior).
pub fn manifest_path_for(arg: &Path) -> Option<(PathBuf, PathBuf)> {
    if arg.is_dir() {
        let manifest = arg.join(MANIFEST_FILE_NAME);
        if manifest.is_file() {
            return Some((arg.to_path_buf(), manifest));
        }
        return None;
    }
    if arg.is_file() && arg.file_name().and_then(|name| name.to_str()) == Some(MANIFEST_FILE_NAME) {
        let dir = arg.parent().map(Path::to_path_buf).unwrap_or_default();
        return Some((dir, arg.to_path_buf()));
    }
    None
}

/// Parse and validate the manifest at `manifest_path` (whose directory is `dir`),
/// then resolve dependencies transitively to build the search-directory list.
pub fn load_manifest(
    dir: &Path,
    manifest_path: &Path,
) -> Result<ResolvedProject, Box<DiagnosticReport>> {
    let mut visited = HashSet::new();
    let mut search_dirs = Vec::new();
    let manifest = parse_manifest(manifest_path)?;

    // Record this project's own src dirs first.
    collect_src_dirs(dir, &manifest, manifest_path, &mut search_dirs)?;

    // Then resolve each dependency's project root and append its src dirs.
    resolve_dependencies(dir, &manifest, &mut visited, &mut search_dirs)?;

    let entry = manifest.entry.as_ref().map(|entry| dir.join(entry));

    Ok(ResolvedProject {
        manifest,
        entry,
        search_dirs,
    })
}

/// Read and JSON-parse a manifest file into a [`ProjectManifest`].
fn parse_manifest(manifest_path: &Path) -> Result<ProjectManifest, Box<DiagnosticReport>> {
    let text = std::fs::read_to_string(manifest_path).map_err(|error| {
        manifest_error(
            format!(
                "failed to read project manifest `{}`: {error}",
                manifest_path.display()
            ),
            manifest_path,
        )
    })?;
    let manifest = serde_json::from_str::<ProjectManifest>(&text).map_err(|error| {
        manifest_error(
            format!(
                "failed to parse project manifest `{}`: {error}",
                manifest_path.display()
            ),
            manifest_path,
        )
    })?;

    if let Some(version) = &manifest.version
        && let Err(reason) = validate_version(version)
    {
        return Err(manifest_error(
            format!(
                "project manifest `{}` has an invalid `version` \"{version}\": it {reason}; \
                 use a semver-shaped MAJOR.MINOR.PATCH, optionally with a `-status` suffix \
                 (e.g. \"0.1.0\" or \"1.0.0-preview\")",
                manifest_path.display()
            ),
            manifest_path,
        ));
    }

    Ok(manifest)
}

/// Validate a manifest `version` string. A well-formed version is a semver-shaped
/// `MAJOR.MINOR.PATCH` core — exactly three `.`-separated non-negative integers,
/// each digits-only with no leading zero (except the single digit `0`) — with an
/// optional `-<prerelease>` suffix of one or more `.`-separated identifiers, each
/// a non-empty run of ASCII letters, digits, or `-` (e.g. `-preview`,
/// `-experimental.2`). Returns `Ok(())` for a well-formed version, or `Err(reason)`
/// describing the first problem for the diagnostic message.
fn validate_version(version: &str) -> Result<(), String> {
    let (core, prerelease) = match version.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease)),
        None => (version, None),
    };

    let mut components = core.split('.');
    for label in ["major", "minor", "patch"] {
        match components.next() {
            Some(component) => validate_numeric_component(component, label)?,
            None => {
                return Err(format!(
                    "is missing the {label} number (expected MAJOR.MINOR.PATCH)"
                ));
            }
        }
    }
    if components.next().is_some() {
        return Err(format!(
            "has too many `.`-separated numbers (expected exactly three: MAJOR.MINOR.PATCH), found {}",
            core.split('.').count()
        ));
    }

    if let Some(prerelease) = prerelease {
        if prerelease.is_empty() {
            return Err("has an empty pre-release suffix after `-`".to_string());
        }
        for identifier in prerelease.split('.') {
            if identifier.is_empty() {
                return Err(
                    "has an empty pre-release identifier (a leading, trailing, or doubled `.`)"
                        .to_string(),
                );
            }
            if !identifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
            {
                return Err(format!(
                    "has an invalid pre-release identifier `{identifier}` \
                     (use ASCII letters, digits, or `-`)"
                ));
            }
        }
    }

    Ok(())
}

/// Validate one `MAJOR`/`MINOR`/`PATCH` numeric component: non-empty, digits only,
/// and no leading zero unless it is the single digit `0`.
fn validate_numeric_component(component: &str, label: &str) -> Result<(), String> {
    if component.is_empty() {
        return Err(format!("has an empty {label} number"));
    }
    if !component.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "has a non-numeric {label} number `{component}` (expected digits only)"
        ));
    }
    if component.len() > 1 && component.starts_with('0') {
        return Err(format!(
            "has a {label} number `{component}` with a leading zero"
        ));
    }
    Ok(())
}

/// Append the (validated, existing) `src` directories of a manifest to `out`.
fn collect_src_dirs(
    dir: &Path,
    manifest: &ProjectManifest,
    manifest_path: &Path,
    out: &mut Vec<PathBuf>,
) -> Result<(), Box<DiagnosticReport>> {
    for src in &manifest.src {
        let src_dir = dir.join(src);
        if !src_dir.is_dir() {
            return Err(manifest_error(
                format!(
                    "project `{}` names a `src` directory `{}` that does not exist (resolved to `{}`)",
                    manifest.name,
                    src,
                    src_dir.display()
                ),
                manifest_path,
            ));
        }
        if !out.iter().any(|existing| existing == &src_dir) {
            out.push(src_dir);
        }
    }
    Ok(())
}

/// Resolve every dependency transitively, appending each dependency's `src`
/// directories to `out`. Cycles between projects are harmless (a project already
/// visited is simply skipped).
fn resolve_dependencies(
    dir: &Path,
    manifest: &ProjectManifest,
    visited: &mut HashSet<PathBuf>,
    out: &mut Vec<PathBuf>,
) -> Result<(), Box<DiagnosticReport>> {
    for (dep_name, dep_path) in &manifest.dependencies {
        let dep_root = dir.join(dep_path);
        let dep_manifest_path = dep_root.join(MANIFEST_FILE_NAME);

        if !dep_root.is_dir() {
            return Err(manifest_error(
                format!(
                    "dependency `{dep_name}` of project `{}` points to `{}`, which is not a directory (resolved to `{}`)",
                    manifest.name,
                    dep_path,
                    dep_root.display()
                ),
                &dep_manifest_path,
            ));
        }
        if !dep_manifest_path.is_file() {
            return Err(manifest_error(
                format!(
                    "dependency `{dep_name}` of project `{}` at `{}` has no `{MANIFEST_FILE_NAME}`",
                    manifest.name,
                    dep_root.display()
                ),
                &dep_manifest_path,
            ));
        }

        // Canonicalize where possible so the same project reached by two paths is
        // visited once; fall back to the joined path if canonicalization fails.
        let key = std::fs::canonicalize(&dep_root).unwrap_or_else(|_| dep_root.clone());
        if !visited.insert(key) {
            continue;
        }

        let dep_manifest = parse_manifest(&dep_manifest_path)?;
        collect_src_dirs(&dep_root, &dep_manifest, &dep_manifest_path, out)?;
        resolve_dependencies(&dep_root, &dep_manifest, visited, out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Create a unique, empty temporary directory for a test and return its path.
    /// Uses the process id and a monotonic counter so parallel tests never collide.
    fn temp_project_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "lullaby_loader_manifest_test_{}_{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("src")).expect("create temp project dir");
        dir
    }

    /// Write a `lullaby.json` with the given body into `dir` and load it, mapping
    /// any failure to its `(code, message)` for assertions.
    fn write_and_load(
        dir: &Path,
        manifest_body: &str,
    ) -> Result<ResolvedProject, (String, String)> {
        let manifest_path = dir.join(MANIFEST_FILE_NAME);
        std::fs::write(&manifest_path, manifest_body).expect("write manifest");
        load_manifest(dir, &manifest_path)
            .map_err(|report| (report.code.clone(), report.message.clone()))
    }

    #[test]
    fn manifest_with_valid_version_parses_and_exposes_it() {
        let dir = temp_project_dir();
        let project = write_and_load(
            &dir,
            "{\n  \"name\": \"withver\",\n  \"version\": \"1.2.0\",\n  \"src\": [\"src\"]\n}\n",
        )
        .expect("valid version should load");
        assert_eq!(project.manifest.version.as_deref(), Some("1.2.0"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn manifest_with_prerelease_version_parses() {
        let dir = temp_project_dir();
        let project = write_and_load(
            &dir,
            "{\n  \"name\": \"pre\",\n  \"version\": \"1.0.0-preview\",\n  \"src\": [\"src\"]\n}\n",
        )
        .expect("prerelease version should load");
        assert_eq!(project.manifest.version.as_deref(), Some("1.0.0-preview"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn manifest_without_version_still_loads() {
        let dir = temp_project_dir();
        let project = write_and_load(
            &dir,
            "{\n  \"name\": \"noversion\",\n  \"src\": [\"src\"]\n}\n",
        )
        .expect("manifest without version must remain backward-compatible");
        assert_eq!(project.manifest.version, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn manifest_with_malformed_version_is_rejected() {
        for bad in [
            "1.2",        // too few components
            "1.2.3.4",    // too many components
            "1.2.x",      // non-numeric
            "1.02.0",     // leading zero
            "v1.2.0",     // stray prefix -> non-numeric major
            "1.2.0-",     // empty prerelease
            "1.2.0-bad!", // invalid prerelease character
        ] {
            let dir = temp_project_dir();
            let body = format!(
                "{{\n  \"name\": \"badver\",\n  \"version\": \"{bad}\",\n  \"src\": [\"src\"]\n}}\n"
            );
            let (code, message) = write_and_load(&dir, &body)
                .expect_err(&format!("version `{bad}` should be rejected"));
            assert_eq!(code, "L0343", "wrong diagnostic code for `{bad}`");
            assert!(
                message.contains("invalid `version`"),
                "expected an invalid-version diagnostic for `{bad}`, got: {message}"
            );
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn validate_version_accepts_well_formed() {
        for good in [
            "0.1.0",
            "1.0.0",
            "10.20.30",
            "1.0.0-preview",
            "2.0.0-experimental.2",
        ] {
            assert!(
                validate_version(good).is_ok(),
                "expected `{good}` to be accepted"
            );
        }
    }

    #[test]
    fn validate_version_rejects_malformed() {
        for bad in ["1.0", "1.0.0.0", "1.0.a", "01.0.0", "1.0.0-", "1.0.0-a..b"] {
            assert!(
                validate_version(bad).is_err(),
                "expected `{bad}` to be rejected"
            );
        }
    }
}
