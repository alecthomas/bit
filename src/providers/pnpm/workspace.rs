use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dashmap::DashMap;
use serde::Deserialize;

use crate::file_tracker::FileTracker;
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
    /// The directories and tarballs its `workspace:`, `link:` and `file:`
    /// dependencies resolve to, directly or through other dependencies, as
    /// the lockfile records them.
    pub local_dependencies: Vec<PathBuf>,
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

    /// The directories and tarballs the projects in `dirs` build from: the
    /// projects themselves, and transitively every local dependency, which
    /// may be another project with local dependencies of its own.
    pub fn local_sources(&self, dirs: &[&Path]) -> Vec<PathBuf> {
        let mut sources: Vec<PathBuf> = Vec::new();
        let mut pending: Vec<PathBuf> = dirs.iter().map(|dir| dir.to_path_buf()).collect();
        while let Some(path) = pending.pop() {
            let path = path.canonicalize().unwrap_or(path);
            if sources.contains(&path) {
                continue;
            }
            if let Some(project) = self.projects.iter().find(|project| project.dir == path) {
                pending.extend(project.local_dependencies.iter().cloned());
            }
            sources.push(path);
        }
        sources.sort();
        sources
    }
}

/// Loaded workspaces by canonical root, shared by every pnpm block.
///
/// The file tracker owns it as a [`FileTracker::derived`] cache, which it
/// drops whenever a block may have written files: `pnpm.install` rewrites
/// the lockfile the dependency graph comes from, and a loaded workspace
/// cannot tell it is stale.
#[derive(Default)]
pub struct Workspaces(DashMap<PathBuf, Arc<Workspace>>);

/// The workspace pnpm uses when run in `dir`, loaded once into `tracker`'s
/// cache. The map is locked only to copy an entry in or out, so two blocks
/// that miss on the same workspace may both load it.
pub fn workspace(tracker: &FileTracker, dir: &Path) -> Result<Arc<Workspace>, BoxError> {
    let root = root_of(dir)?;
    let workspaces = tracker.derived::<Workspaces>();
    if let Some(workspace) = workspaces.0.get(&root) {
        return Ok(Arc::clone(&workspace));
    }
    let workspace = Arc::new(load_workspace(&root)?);
    Ok(workspaces.0.entry(root).or_insert(workspace).value().clone())
}

/// The workspace pnpm uses when run in `dir`, loaded without a cache.
pub fn load(dir: &Path) -> Result<Workspace, BoxError> {
    load_workspace(&root_of(dir)?)
}

/// The canonical root of the workspace pnpm uses when run in `dir`.
fn root_of(dir: &Path) -> Result<PathBuf, BoxError> {
    let dir = dir
        .canonicalize()
        .map_err(|e| format!("pnpm: canonicalizing {}: {e}", dir.display()))?;
    Ok(workspace_root(&dir, |name| std::env::var_os(name)))
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
#[serde(rename_all = "camelCase")]
struct ListedProject {
    #[serde(default)]
    name: Option<String>,
    path: PathBuf,
    #[serde(default)]
    dependencies: HashMap<String, ListedDependency>,
    #[serde(default)]
    dev_dependencies: HashMap<String, ListedDependency>,
    #[serde(default)]
    optional_dependencies: HashMap<String, ListedDependency>,
}

/// A dependency as `pnpm ls --json` reports it, with its own dependencies.
#[derive(Deserialize)]
struct ListedDependency {
    /// The resolved version: `link:<dir>` for a linked directory or
    /// workspace project, `file:<path>` for a directory or tarball copied
    /// from the filesystem, and otherwise a registry version.
    #[serde(default)]
    version: String,
    #[serde(default)]
    path: Option<PathBuf>,
    #[serde(default)]
    dependencies: HashMap<String, ListedDependency>,
}

/// The workspace's projects, as pnpm finds them from its `packages`
/// patterns, which follow fast-glob's rules rather than the glob crate's,
/// with the local dependencies its lockfile resolves.
fn list_projects(root: &Path) -> Result<Vec<Project>, BoxError> {
    // `--lockfile-only` reads the graph without an install, and only pnpm
    // 10 and later accept it. Without it only the projects are listed.
    let graph = ["ls", "--recursive", "--depth", "Infinity", "--json", "--lockfile-only"];
    let listed = match root.join("pnpm-lock.yaml").is_file() {
        true => pnpm_ls(root, &graph).or_else(|_| pnpm_ls(root, &["ls", "--recursive", "--depth", "-1", "--json"])),
        false => pnpm_ls(root, &["ls", "--recursive", "--depth", "-1", "--json"]),
    }?;
    let mut projects: Vec<Project> = listed
        .into_iter()
        .map(|project| {
            let mut local_dependencies = Vec::new();
            for dependency in project
                .dependencies
                .values()
                .chain(project.dev_dependencies.values())
                .chain(project.optional_dependencies.values())
            {
                collect_local(root, dependency, &mut local_dependencies);
            }
            local_dependencies.sort();
            local_dependencies.dedup();
            Project {
                dir: project.path.canonicalize().unwrap_or(project.path),
                name: project.name,
                local_dependencies,
            }
        })
        .collect();
    projects.sort_by(|a, b| a.dir.cmp(&b.dir));
    Ok(projects)
}

/// The projects `pnpm <args>` lists in `root`.
fn pnpm_ls(root: &Path, args: &[&str]) -> Result<Vec<ListedProject>, BoxError> {
    let output = std::process::Command::new("pnpm")
        .args(args)
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
    Ok(serde_json::from_slice(&output.stdout).map_err(|e| format!("pnpm: parsing `pnpm ls` output: {e}"))?)
}

/// Add the local paths `dependency` and its own dependencies resolve to.
/// A `link:` dependency reports its directory as `path`. A `file:` one is
/// copied into the virtual store, so its `path` is the copy, and its version
/// names the original relative to the lockfile's directory, `root`.
fn collect_local(root: &Path, dependency: &ListedDependency, paths: &mut Vec<PathBuf>) {
    if dependency.version.starts_with("link:") {
        paths.extend(dependency.path.clone().filter(|path| path.is_absolute()));
    } else if let Some(file) = dependency.version.strip_prefix("file:") {
        paths.push(root.join(file));
    }
    for nested in dependency.dependencies.values() {
        collect_local(root, nested, paths);
    }
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

    #[test]
    fn loads_workspace_packages() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        setup_workspace(root);

        let ws = load(root).unwrap();
        assert!(ws.is_workspace());
        assert!(ws.lockfile.is_some());
        // pnpm-workspace.yaml lists bff + frontend, and pnpm always counts
        // the root as a project.
        let names: Vec<_> = ws.projects.iter().map(|p| p.name.as_deref()).collect();
        assert_eq!(names, [Some("root"), Some("bff"), Some("frontend")]);
    }

    #[test]
    fn the_tracker_cache_reloads_after_invalidation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        setup_workspace(root);
        let tracker = FileTracker::new();

        assert_eq!(workspace(&tracker, root).unwrap().projects.len(), 3);
        fs::write(root.join("pnpm-workspace.yaml"), "packages:\n  - bff\n").unwrap();
        assert_eq!(workspace(&tracker, root).unwrap().projects.len(), 3, "cached");
        tracker.invalidate();
        assert_eq!(workspace(&tracker, root).unwrap().projects.len(), 2);
    }

    #[test]
    fn local_dependencies_are_followed_transitively() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("package.json"), r#"{ "name": "root" }"#).unwrap();
        fs::write(root.join("pnpm-workspace.yaml"), "packages:\n  - 'packages/*'\n").unwrap();
        for (dir, manifest) in [
            (
                "packages/app",
                r#"{ "name": "app", "dependencies": { "lib": "workspace:*" } }"#,
            ),
            (
                "packages/lib",
                r#"{ "name": "lib", "devDependencies": { "ext": "file:../../ext", "linked": "link:../../linked" } }"#,
            ),
            ("packages/other", r#"{ "name": "other" }"#),
            (
                "ext",
                r#"{ "name": "ext", "version": "1.0.0", "dependencies": { "deep": "file:../deep" } }"#,
            ),
            ("deep", r#"{ "name": "deep", "version": "1.0.0" }"#),
            ("linked", r#"{ "name": "linked", "version": "1.0.0" }"#),
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
            fs::write(root.join(dir).join("package.json"), manifest).unwrap();
        }
        let install = std::process::Command::new("pnpm")
            .args(["install", "--lockfile-only", "--offline"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(install.status.success(), "{}", String::from_utf8_lossy(&install.stderr));

        let ws = load(&root).unwrap();
        let app = root.join("packages/app");
        let expected: Vec<PathBuf> = ["deep", "ext", "linked", "packages/app", "packages/lib"]
            .iter()
            .map(|dir| root.join(dir))
            .collect();
        assert_eq!(ws.local_sources(&[&app]), expected);
    }

    #[test]
    fn unnamed_and_duplicate_projects_are_kept() {
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

        let ws = load(&root).unwrap();
        assert!(
            ws.projects
                .iter()
                .any(|p| p.dir == root.join("packages/unnamed") && p.name.is_none())
        );
        assert_eq!(
            ws.package_dirs("dup"),
            [root.join("packages/a"), root.join("packages/b")]
        );
        assert_eq!(packages_with_script(&ws, "test").unwrap(), ["dup"]);
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
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        setup_workspace(&root);

        let ws = load(&root.join("bff")).unwrap();
        assert_eq!(ws.root, root);
        assert_eq!(ws.lockfile.as_deref(), Some(root.join("pnpm-lock.yaml").as_path()));
    }

    #[test]
    fn standalone_package_without_workspace_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("package.json"), r#"{ "name": "solo" }"#).unwrap();

        let ws = load(root).unwrap();
        assert!(!ws.is_workspace());
        assert!(ws.lockfile.is_none());
        assert_eq!(ws.package_dirs("solo"), [root.canonicalize().unwrap()]);
    }

    #[test]
    fn packages_with_script_filters_and_sorts_packages() {
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

        assert_eq!(packages_with_script(&load(root).unwrap(), "test").unwrap(), vec!["bff"]);
    }

    #[test]
    fn missing_package_json_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_err());
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

        let ws = load(root).unwrap();
        assert_eq!(ws.package_dirs("a").len(), 1);
        assert_eq!(ws.package_dirs("b").len(), 1);
        assert!(ws.package_dirs("ignored").is_empty());
    }
}
