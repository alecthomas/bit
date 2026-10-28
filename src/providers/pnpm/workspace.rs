use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Deserialize;

use crate::process::CommandExt;
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
    /// Every project pnpm finds in the workspace, the root included, sorted
    /// by directory. Keyed by directory rather than name, since a name may
    /// be missing or shared by several projects.
    pub projects: Vec<Project>,
}

/// A workspace project: a directory with a `package.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    /// Canonical directory.
    pub dir: PathBuf,
    /// The `name` its `package.json` gives, if any.
    pub name: Option<String>,
}

impl Workspace {
    /// Whether this is a multi-package workspace (has `pnpm-workspace.yaml`).
    pub fn is_workspace(&self) -> bool {
        self.workspace_yaml.is_some()
    }

    /// The directories of the projects named `name`, which `--filter name`
    /// selects. Several projects may share a name.
    pub fn package_dirs(&self, name: &str) -> Vec<&Path> {
        self.projects
            .iter()
            .filter(|project| project.name.as_deref() == Some(name))
            .map(|project| project.dir.as_path())
            .collect()
    }
}

static CACHE: Mutex<Option<Workspace>> = Mutex::new(None);

/// Acquire the workspace pnpm uses when run in `dir`, loading it on first
/// call and reusing the cached copy while the same root is used. Mirrors
/// the go provider's per-module-root cache pattern.
pub fn with_workspace<F, R>(dir: &Path, f: F) -> Result<R, BoxError>
where
    F: FnOnce(&Workspace) -> Result<R, BoxError>,
{
    let dir = dir
        .canonicalize()
        .map_err(|e| format!("pnpm: canonicalizing {}: {e}", dir.display()))?;
    let root = workspace_root(&dir, |name| std::env::var_os(name));
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

/// The root of the workspace pnpm uses when run in `dir`, as pnpm's
/// `find-workspace-dir` finds it: the directory `NPM_CONFIG_WORKSPACE_DIR`
/// names, or else the nearest directory at or above `dir` holding a
/// `pnpm-workspace.yaml`. Outside a workspace, `dir` is its own root.
fn workspace_root(dir: &Path, var: impl Fn(&str) -> Option<std::ffi::OsString>) -> PathBuf {
    if let Some(root) = var("NPM_CONFIG_WORKSPACE_DIR").or_else(|| var("npm_config_workspace_dir")) {
        let root = dir.join(root);
        return root.canonicalize().unwrap_or(root);
    }
    dir.ancestors()
        .find(|ancestor| ancestor.join("pnpm-workspace.yaml").is_file())
        .unwrap_or(dir)
        .to_path_buf()
}

fn load_workspace(root: &Path) -> Result<Workspace, BoxError> {
    let root_package_json = root.join("package.json");
    if !root_package_json.exists() {
        return Err(format!("pnpm: no package.json at {}", root.display()).into());
    }

    let lockfile_path = root.join("pnpm-lock.yaml");
    let lockfile = lockfile_path.exists().then_some(lockfile_path);

    let ws_yaml = root.join("pnpm-workspace.yaml");
    let workspace_yaml = ws_yaml.exists().then_some(ws_yaml);

    Ok(Workspace {
        root: root.to_path_buf(),
        lockfile,
        workspace_yaml,
        root_package_json,
        projects: list_projects(root)?,
    })
}

/// A project as `pnpm ls --json` reports it.
#[derive(Deserialize)]
struct ListedProject {
    #[serde(default)]
    name: Option<String>,
    path: PathBuf,
}

/// The workspace's projects, as pnpm finds them from its `packages`
/// patterns, which follow fast-glob's rules rather than the glob crate's.
fn list_projects(root: &Path) -> Result<Vec<Project>, BoxError> {
    let output = std::process::Command::new("pnpm")
        .args(["ls", "--recursive", "--depth", "-1", "--json"])
        .current_dir(root)
        .logged_output()
        .map_err(|e| format!("pnpm: failed to execute `pnpm ls`: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "pnpm: `pnpm ls` in {} failed: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    let listed: Vec<ListedProject> =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("pnpm: parsing `pnpm ls` output: {e}"))?;
    let mut projects: Vec<Project> = listed
        .into_iter()
        .map(|project| Project {
            dir: project.path.canonicalize().unwrap_or(project.path),
            name: project.name,
        })
        .collect();
    projects.sort_by(|a, b| a.dir.cmp(&b.dir));
    Ok(projects)
}

#[derive(Deserialize)]
struct PackageJsonScripts {
    #[serde(default)]
    scripts: HashMap<String, String>,
}

/// The names of the projects that define `script`, as `pnpm --recursive run`
/// would run it: a workspace's root project is left out unless it is the
/// only one.
pub fn packages_with_script(workspace: &Workspace, script: &str) -> Result<Vec<String>, BoxError> {
    let mut packages = Vec::new();
    for project in &workspace.projects {
        let Some(name) = &project.name else { continue };
        if workspace.is_workspace() && project.dir == workspace.root {
            continue;
        }
        let path = project.dir.join("package.json");
        let content = fs::read_to_string(&path).map_err(|e| format!("pnpm: reading {}: {e}", path.display()))?;
        let package: PackageJsonScripts =
            serde_json::from_str(&content).map_err(|e| format!("pnpm: parsing {}: {e}", path.display()))?;
        if package.scripts.contains_key(script) {
            packages.push(name.clone());
        }
    }
    packages.sort();
    packages.dedup();
    Ok(packages)
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
            // pnpm-workspace.yaml lists bff + frontend, and pnpm always
            // counts the root as a project.
            let names: Vec<_> = ws.projects.iter().map(|p| p.name.as_deref()).collect();
            assert_eq!(names, [Some("root"), Some("bff"), Some("frontend")]);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn unnamed_and_duplicate_projects_are_kept() {
        reset_cache();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        setup_workspace(&root);
        fs::write(root.join("pnpm-workspace.yaml"), "packages:\n  - 'packages/*'\n").unwrap();
        for (dir, manifest) in [
            ("packages/unnamed", "{}"),
            ("packages/a", r#"{ "name": "dup", "scripts": { "test": "t" } }"#),
            ("packages/b", r#"{ "name": "dup", "scripts": { "test": "t" } }"#),
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
            fs::write(root.join(dir).join("package.json"), manifest).unwrap();
        }

        with_workspace(&root, |ws| {
            assert!(
                ws.projects
                    .iter()
                    .any(|p| p.dir == root.join("packages/unnamed") && p.name.is_none())
            );
            assert_eq!(
                ws.package_dirs("dup"),
                [root.join("packages/a"), root.join("packages/b")]
            );
            assert_eq!(packages_with_script(ws, "test")?, ["dup"]);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn workspace_root_is_found_upward_or_named_by_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        setup_workspace(&root);
        let no_env = |_: &str| None;
        assert_eq!(workspace_root(&root.join("bff/src"), no_env), root);
        assert_eq!(workspace_root(&root, no_env), root);

        let solo = tempfile::tempdir().unwrap();
        let solo = solo.path().canonicalize().unwrap();
        assert_eq!(workspace_root(&solo, no_env), solo);

        let env = |name: &str| (name == "npm_config_workspace_dir").then(|| root.clone().into_os_string());
        assert_eq!(workspace_root(&solo, env), root);
    }

    #[test]
    fn subdirectory_uses_the_workspace_lockfile() {
        reset_cache();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        setup_workspace(&root);

        with_workspace(&root.join("bff"), |ws| {
            assert_eq!(ws.root, root);
            assert_eq!(ws.lockfile.as_deref(), Some(root.join("pnpm-lock.yaml").as_path()));
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
            assert_eq!(ws.package_dirs("solo"), [root.canonicalize().unwrap()]);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn packages_with_script_filters_and_sorts_packages() {
        reset_cache();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        setup_workspace(root);
        fs::write(
            root.join("bff/package.json"),
            r#"{ "name": "bff", "scripts": { "test": "vitest" } }"#,
        )
        .unwrap();
        fs::write(
            root.join("frontend/package.json"),
            r#"{ "name": "frontend", "scripts": { "build": "vite build" } }"#,
        )
        .unwrap();

        with_workspace(root, |workspace| {
            assert_eq!(packages_with_script(workspace, "test")?, vec!["bff"]);
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
            assert_eq!(ws.package_dirs("a").len(), 1);
            assert_eq!(ws.package_dirs("b").len(), 1);
            assert!(ws.package_dirs("ignored").is_empty());
            Ok(())
        })
        .unwrap();
    }
}
