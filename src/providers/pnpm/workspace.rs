use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Deserialize;

use crate::provider::BoxError;

/// A discovered pnpm workspace (or a single-package project).
#[derive(Debug, Clone)]
pub struct Workspace {
    /// Canonicalized workspace root directory.
    pub root: PathBuf,
    /// `pnpm-lock.yaml` path if it exists on disk.
    pub lockfile: Option<PathBuf>,
    /// `pnpm-workspace.yaml` path if this is a workspace.
    pub workspace_yaml: Option<PathBuf>,
    /// Root `package.json` path.
    pub root_package_json: PathBuf,
    /// Map of package `name` (from each `package.json`) to its directory.
    /// Includes the root package when the root `package.json` has a name.
    pub packages: HashMap<String, PathBuf>,
}

impl Workspace {
    /// Whether this is a multi-package workspace (has `pnpm-workspace.yaml`).
    pub fn is_workspace(&self) -> bool {
        self.workspace_yaml.is_some()
    }

    /// Directory for a package by name. If the workspace has no sub-packages,
    /// any lookup falls back to the root.
    pub fn package_dir(&self, name: &str) -> Option<&Path> {
        self.packages.get(name).map(|p| p.as_path())
    }
}

static CACHE: Mutex<Option<Workspace>> = Mutex::new(None);

/// Acquire the workspace for `dir`, loading it on first call and reusing
/// the cached copy while the same root is used. Mirrors the go provider's
/// per-module-root cache pattern.
pub fn with_workspace<F, R>(dir: &Path, f: F) -> Result<R, BoxError>
where
    F: FnOnce(&Workspace) -> Result<R, BoxError>,
{
    let root = dir
        .canonicalize()
        .map_err(|e| format!("pnpm: canonicalizing {}: {e}", dir.display()))?;
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let needs_init = match &*guard {
        Some(ws) => ws.root != root,
        None => true,
    };
    if needs_init {
        *guard = Some(load_workspace(&root)?);
    }
    f(guard.as_ref().expect("workspace initialized above"))
}

fn load_workspace(root: &Path) -> Result<Workspace, BoxError> {
    let root_package_json = root.join("package.json");
    if !root_package_json.exists() {
        return Err(format!("pnpm: no package.json at {}", root.display()).into());
    }

    let lockfile_path = root.join("pnpm-lock.yaml");
    let lockfile = lockfile_path.exists().then_some(lockfile_path);

    let ws_yaml = root.join("pnpm-workspace.yaml");
    let (workspace_yaml, package_dirs): (Option<PathBuf>, Vec<PathBuf>) = if ws_yaml.exists() {
        let patterns = parse_workspace_yaml(&ws_yaml)?;
        let dirs = expand_workspace_patterns(root, &patterns)?;
        (Some(ws_yaml), dirs)
    } else {
        (None, vec![root.to_path_buf()])
    };

    let mut packages = HashMap::new();
    for dir in &package_dirs {
        let pkg_json = dir.join("package.json");
        if !pkg_json.exists() {
            continue;
        }
        if let Some(name) = parse_package_name(&pkg_json)? {
            packages.insert(name, dir.clone());
        }
    }

    Ok(Workspace {
        root: root.to_path_buf(),
        lockfile,
        workspace_yaml,
        root_package_json,
        packages,
    })
}

#[derive(Deserialize)]
struct WorkspaceYaml {
    #[serde(default)]
    packages: Vec<String>,
}

fn parse_workspace_yaml(path: &Path) -> Result<Vec<String>, BoxError> {
    let content = fs::read_to_string(path).map_err(|e| format!("pnpm: reading {}: {e}", path.display()))?;
    let ws: WorkspaceYaml =
        serde_yml::from_str(&content).map_err(|e| format!("pnpm: parsing {}: {e}", path.display()))?;
    Ok(ws.packages)
}

/// Expand pnpm-workspace.yaml `packages` glob patterns into actual directories.
/// Exclusion patterns (leading `!`) are honored: dirs matched by an exclusion
/// are removed from the result. Non-directories are ignored.
fn expand_workspace_patterns(root: &Path, patterns: &[String]) -> Result<Vec<PathBuf>, BoxError> {
    let mut included: Vec<PathBuf> = Vec::new();
    let mut excluded: Vec<PathBuf> = Vec::new();
    for pattern in patterns {
        let (list, pat) = match pattern.strip_prefix('!') {
            Some(p) => (&mut excluded, p),
            None => (&mut included, pattern.as_str()),
        };
        let full = root.join(pat);
        for entry in glob::glob(&full.to_string_lossy()).map_err(|e| format!("pnpm: invalid glob {pat:?}: {e}"))? {
            let Ok(path) = entry else { continue };
            if path.is_dir() {
                list.push(path);
            }
        }
    }
    included.retain(|p| !excluded.contains(p));
    Ok(included)
}

#[derive(Deserialize)]
struct PackageJsonName {
    #[serde(default)]
    name: Option<String>,
}

fn parse_package_name(path: &Path) -> Result<Option<String>, BoxError> {
    let content = fs::read_to_string(path).map_err(|e| format!("pnpm: reading {}: {e}", path.display()))?;
    let pkg: PackageJsonName =
        serde_json::from_str(&content).map_err(|e| format!("pnpm: parsing {}: {e}", path.display()))?;
    Ok(pkg.name)
}

/// Directory names that are never considered source inputs.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".turbo",
    ".vite",
    ".svelte-kit",
    "coverage",
    ".cache",
];

/// File names that are never considered source inputs.
const SKIP_FILE_SUFFIXES: &[&str] = &[".tsbuildinfo"];

/// Recursively collect source files under `dir`, skipping common
/// build-output and tooling directories as well as any paths listed in
/// `exclude`. Returns absolute-ish paths (as walked from `dir`).
pub fn scan_sources(dir: &Path, exclude: &[PathBuf]) -> Vec<PathBuf> {
    let exclude_canonical: Vec<PathBuf> = exclude
        .iter()
        .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
        .collect();
    let mut out = Vec::new();
    scan_dir(dir, &exclude_canonical, &mut out);
    out
}

fn scan_dir(dir: &Path, exclude: &[PathBuf], out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        if exclude.contains(&canonical) {
            continue;
        }

        if ft.is_dir() {
            if SKIP_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            scan_dir(&path, exclude, out);
        } else if ft.is_file() {
            if SKIP_FILE_SUFFIXES.iter().any(|s| name_str.ends_with(s)) {
                continue;
            }
            if name_str == ".DS_Store" {
                continue;
            }
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_workspace(root: &Path) {
        fs::write(root.join("package.json"), r#"{ "name": "root" }"#).unwrap();
        fs::write(root.join("pnpm-workspace.yaml"), "packages:\n  - bff\n  - frontend\n").unwrap();
        fs::write(root.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
        fs::create_dir_all(root.join("bff/src")).unwrap();
        fs::write(root.join("bff/package.json"), r#"{ "name": "bff" }"#).unwrap();
        fs::write(root.join("bff/src/index.ts"), "").unwrap();
        fs::create_dir_all(root.join("frontend/src")).unwrap();
        fs::write(root.join("frontend/package.json"), r#"{ "name": "frontend" }"#).unwrap();
        fs::write(root.join("frontend/src/main.tsx"), "").unwrap();
    }

    fn reset_cache() {
        *CACHE.lock().unwrap() = None;
    }

    #[test]
    fn loads_workspace_packages() {
        reset_cache();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        setup_workspace(root);

        with_workspace(root, |ws| {
            assert!(ws.is_workspace());
            assert!(ws.lockfile.is_some());
            // pnpm-workspace.yaml lists bff + frontend. The root itself isn't
            // a package unless listed.
            assert_eq!(ws.packages.len(), 2);
            assert!(ws.packages.contains_key("bff"));
            assert!(ws.packages.contains_key("frontend"));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn standalone_package_without_workspace_yaml() {
        reset_cache();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("package.json"), r#"{ "name": "solo" }"#).unwrap();

        with_workspace(root, |ws| {
            assert!(!ws.is_workspace());
            assert!(ws.lockfile.is_none());
            assert_eq!(ws.packages.len(), 1);
            assert!(ws.packages.contains_key("solo"));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn missing_package_json_errors() {
        reset_cache();
        let dir = tempfile::tempdir().unwrap();
        let err = with_workspace(dir.path(), |_| Ok(()));
        assert!(err.is_err());
    }

    #[test]
    fn scan_sources_excludes_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("node_modules/foo")).unwrap();
        fs::create_dir_all(root.join("dist")).unwrap();
        fs::write(root.join("src/index.ts"), "").unwrap();
        fs::write(root.join("package.json"), "{}").unwrap();
        fs::write(root.join("node_modules/foo/bar.js"), "").unwrap();
        fs::write(root.join("dist/bundle.js"), "").unwrap();

        let files = scan_sources(root, &[]);
        let names: Vec<&str> = files
            .iter()
            .filter_map(|p| p.file_name().and_then(|s| s.to_str()))
            .collect();
        assert!(names.contains(&"index.ts"));
        assert!(names.contains(&"package.json"));
        assert!(!names.contains(&"bar.js"));
        assert!(!names.contains(&"bundle.js"));
    }

    #[test]
    fn scan_sources_honors_exclude_list() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("generated")).unwrap();
        fs::write(root.join("src/index.ts"), "").unwrap();
        fs::write(root.join("generated/out.ts"), "").unwrap();

        let exclude = vec![root.join("generated")];
        let files = scan_sources(root, &exclude);
        let names: Vec<&str> = files
            .iter()
            .filter_map(|p| p.file_name().and_then(|s| s.to_str()))
            .collect();
        assert!(names.contains(&"index.ts"));
        assert!(!names.contains(&"out.ts"));
    }

    #[test]
    fn workspace_glob_patterns() {
        reset_cache();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("package.json"), r#"{ "name": "root" }"#).unwrap();
        fs::write(
            root.join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n  - '!packages/ignored'\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("packages/a")).unwrap();
        fs::write(root.join("packages/a/package.json"), r#"{ "name": "a" }"#).unwrap();
        fs::create_dir_all(root.join("packages/b")).unwrap();
        fs::write(root.join("packages/b/package.json"), r#"{ "name": "b" }"#).unwrap();
        fs::create_dir_all(root.join("packages/ignored")).unwrap();
        fs::write(root.join("packages/ignored/package.json"), r#"{ "name": "ignored" }"#).unwrap();

        with_workspace(root, |ws| {
            assert!(ws.packages.contains_key("a"));
            assert!(ws.packages.contains_key("b"));
            assert!(!ws.packages.contains_key("ignored"));
            Ok(())
        })
        .unwrap();
    }
}
