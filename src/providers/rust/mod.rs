pub mod build;
pub mod clippy;
pub mod exe;
pub mod fmt;
pub mod test;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex};

use serde::{Deserialize, Serialize};

use crate::cache::ProjectIdentity;
use crate::file_tracker::FileTracker;
use crate::provider::{BoxError, DynResource, FuncSignature, Provider};
use crate::sha256::SHA256;
use crate::value::{BlockRef, Value};

/// Shared Rust environment/config fields flattened into all rust resources.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, bit_derive::Schema)]
pub struct RustEnv {
    /// Target triple (e.g. "x86_64-unknown-linux-musl")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Build profile (e.g. "release")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Rust toolchain (e.g. "nightly")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<String>,
}

impl RustEnv {
    /// Start building a cargo command for the given subcommand.
    pub fn cargo(&self, subcommand: &str) -> CargoCommand {
        let program = if let Some(tc) = &self.toolchain {
            format!("cargo+{tc}")
        } else {
            "cargo".into()
        };
        let mut args = vec![subcommand.to_owned()];
        if let Some(t) = &self.target {
            args.extend(["--target".into(), t.clone()]);
        }
        if let Some(p) = &self.profile {
            args.extend(["--profile".into(), p.clone()]);
        }
        CargoCommand { program, args }
    }
}

/// Shared Rust feature flags used across build/test/clippy resources.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, bit_derive::Schema)]
pub struct RustFeatures {
    /// Features to enable
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// Enable all features
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all_features: bool,
}

/// A cargo command builder that can produce both a `Command` and a display string.
pub struct CargoCommand {
    program: String,
    args: Vec<String>,
}

impl CargoCommand {
    /// Create a new CargoCommand with the given program name.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    /// Append a single argument.
    pub fn arg(&mut self, arg: impl Into<String>) -> &mut Self {
        self.args.push(arg.into());
        self
    }

    /// Append two arguments (e.g. a flag and its value).
    pub fn arg2(&mut self, flag: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.args.push(flag.into());
        self.args.push(value.into());
        self
    }

    /// Append feature flags.
    pub fn features(&mut self, features: &RustFeatures) -> &mut Self {
        if features.all_features {
            self.args.push("--all-features".into());
        } else if !features.features.is_empty() {
            self.args.push("--features".into());
            self.args.push(features.features.join(","));
        }
        self
    }

    /// Append extra user-provided flags.
    pub fn extra_flags(&mut self, flags: &[String]) -> &mut Self {
        self.args.extend(flags.iter().cloned());
        self
    }

    /// Return the display string (e.g. "cargo build --profile release").
    pub fn display(&self) -> String {
        let mut parts = vec![self.program.clone()];
        parts.extend(self.args.iter().cloned());
        parts.join(" ")
    }

    /// Build a `std::process::Command` ready to spawn. In a linked worktree,
    /// workspace crates are compiled through a wrapper script that remaps
    /// source paths (see [`remap_wrapper`]).
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        if let Some(wrapper) = remap_wrapper()
            && std::env::var_os("RUSTC_WORKSPACE_WRAPPER").is_none()
        {
            cmd.env("RUSTC_WORKSPACE_WRAPPER", wrapper);
        }
        cmd
    }

    /// Spawn the command, pipe stdout/stderr to the writer, wait for exit.
    /// Returns an error if the command fails to spawn or exits non-zero.
    pub fn run(&self, writer: &crate::output::BlockWriter) -> Result<(), BoxError> {
        let mut child = self
            .command()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to execute `{}`: {e}", self.display()))?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        std::thread::scope(|s| {
            if let Some(out) = stdout {
                s.spawn(|| writer.pipe_stdout(std::io::BufReader::new(out)));
            }
            if let Some(err) = stderr {
                s.spawn(|| writer.pipe_stderr(std::io::BufReader::new(err)));
            }
        });

        let status = child
            .wait()
            .map_err(|e| format!("failed to wait for `{}`: {e}", self.display()))?;
        if !status.success() {
            return Err(format!("`{}` exited with {status}", self.display()).into());
        }
        Ok(())
    }
}

/// `--remap-path-prefix` value that rewrites this worktree's root to the
/// repository's main worktree, so binaries built in linked worktrees embed
/// identical paths and their debug info still points at a real checkout.
/// `None` outside Git or when this already is the main worktree.
fn remap_path_prefix() -> Option<String> {
    let root = std::env::current_dir().ok()?.canonicalize().ok()?;
    let main = ProjectIdentity::detect(&root).main_worktree()?;
    if main == root {
        return None;
    }
    Some(format!("{}={}", root.display(), main.display()))
}

/// Shell script cargo runs as `RUSTC_WORKSPACE_WRAPPER`: it execs the rustc
/// command line it is handed with the remap flag appended. A script rather
/// than an environment variable so nothing leaks into build scripts or test
/// binaries that cargo spawns.
fn wrapper_script(remap: &str) -> String {
    let quoted = remap.replace('\'', "'\\''");
    format!(
        "#!/bin/sh\n# Generated by bit: compile workspace crates with remapped source paths.\nexec \"$@\" '--remap-path-prefix={quoted}'\n"
    )
}

/// Path of the remap wrapper script for this worktree, written once per
/// process under the shared cache directory. `None` when no remap applies
/// or the script cannot be written; the build then simply proceeds without
/// remapping.
fn remap_wrapper() -> Option<&'static Path> {
    static WRAPPER: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
        let remap = remap_path_prefix()?;
        let dir = crate::cache::cache_root().ok()?.join("rustc-wrapper");
        let script = wrapper_script(&remap);
        let path = dir.join(format!("{}.sh", SHA256::digest(script.as_bytes())));
        if path.is_file() {
            return Some(path);
        }
        std::fs::create_dir_all(&dir).ok()?;
        let tmp = dir.join(format!(
            ".{}.{}",
            std::process::id(),
            path.file_name()?.to_string_lossy()
        ));
        std::fs::write(&tmp, script).ok()?;
        std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o755)).ok()?;
        if std::fs::rename(&tmp, &path).is_err() && !path.is_file() {
            let _ = std::fs::remove_file(&tmp);
            return None;
        }
        Some(path)
    });
    WRAPPER.as_deref()
}

/// Fingerprint of the Rust toolchain and build environment, for the shared
/// action key: `rustc -vV` for the selected toolchain, the toolchain pin
/// file if present, and the cargo/rustc environment variables that change
/// compiler output.
pub fn toolchain_fingerprint(env: &RustEnv) -> Result<BTreeMap<String, String>, BoxError> {
    const ENV_VARS: [&str; 4] = [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
        "CARGO_BUILD_TARGET",
    ];
    let probe_key = format!("rustc -vV|{:?}", env.toolchain);
    let rustc = super::probe_tool(&probe_key, || {
        let mut cmd = Command::new("rustc");
        cmd.arg("-vV");
        if let Some(tc) = &env.toolchain {
            cmd.env("RUSTUP_TOOLCHAIN", tc);
        }
        cmd
    })?;
    let mut fingerprint = BTreeMap::new();
    fingerprint.insert("rustc".to_owned(), rustc);
    for name in ["rust-toolchain.toml", "rust-toolchain"] {
        let path = Path::new(name);
        if path.is_file() {
            fingerprint.insert(format!("file.{name}"), super::hash_file(path)?.to_string());
        }
    }
    for var in ENV_VARS {
        if let Ok(value) = std::env::var(var) {
            fingerprint.insert(format!("env.{var}"), value);
        }
    }
    Ok(fingerprint)
}

/// Cached source directories, individual files, and target directory
/// discovered by `cargo metadata`. The cache avoids re-running the expensive
/// metadata call on every resolve.
#[derive(Clone)]
struct InputPaths {
    globs: Vec<String>,
    files: Vec<PathBuf>,
}

#[derive(Clone)]
struct DiscoveredPaths {
    workspace: InputPaths,
    packages: HashMap<String, InputPaths>,
    target_dir: PathBuf,
}

static DISCOVERED_CACHE: Mutex<Option<DiscoveredPaths>> = Mutex::new(None);
static CARGO_METADATA_CACHE: Mutex<Option<serde_json::Value>> = Mutex::new(None);

/// Discover once per process and return a copy so the lock is released
/// before any hashing.
fn discovered() -> Result<DiscoveredPaths, BoxError> {
    let mut guard = DISCOVERED_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(discover_paths()?);
    }
    Ok(guard.clone().expect("just populated"))
}

/// Cargo's target directory for the current project, honouring
/// `CARGO_TARGET_DIR` and config.
pub fn target_directory() -> Result<PathBuf, BoxError> {
    Ok(discovered()?.target_dir)
}

/// Resolve Rust source files for change detection.
///
/// Uses `cargo metadata` to discover local package source directories, then
/// hashes `.rs` files within them via the tracker. When `package` is set, only
/// that package and its transitive local workspace dependencies are included.
/// The metadata discovery is cached so the call only happens once per run.
pub fn resolve_rust_inputs(
    package: Option<&str>,
    tracker: &mut FileTracker,
) -> Result<BTreeMap<String, SHA256>, BoxError> {
    let discovered = discovered()?;
    let paths = match package {
        Some(package) => discovered
            .packages
            .get(package)
            .ok_or_else(|| format!("Rust package '{package}' not found in workspace"))?,
        None => &discovered.workspace,
    };

    let mut result = BTreeMap::new();
    for pattern in &paths.globs {
        result.extend(tracker.hash_glob(pattern)?);
    }
    for path in &paths.files {
        if path.is_file() {
            let key = path.display().to_string();
            result.insert(key, tracker.hash_file(path)?);
        }
    }
    Ok(result)
}

/// Discover source globs, individual files, and the target directory from
/// `cargo metadata`.
fn discover_paths() -> Result<DiscoveredPaths, BoxError> {
    let cwd = std::env::current_dir()?;
    let meta = cargo_metadata()?;
    let target_dir = meta
        .get("target_directory")
        .and_then(|t| t.as_str())
        .map(PathBuf::from)
        .ok_or("`cargo metadata` output has no target_directory")?;
    let workspace_packages = workspace_packages_by_name(&meta)?;
    let workspace_values: Vec<_> = workspace_packages.values().copied().collect();
    let workspace = discover_input_paths(&workspace_values, &cwd);
    let mut packages = HashMap::new();
    for name in workspace_packages.keys() {
        let closure = workspace_package_closure(&meta, name)?;
        let values: Vec<_> = closure
            .iter()
            .filter_map(|dependency| workspace_packages.get(dependency).copied())
            .collect();
        packages.insert(name.clone(), discover_input_paths(&values, &cwd));
    }

    Ok(DiscoveredPaths {
        workspace,
        packages,
        target_dir,
    })
}

fn discover_input_paths(packages: &[&serde_json::Value], cwd: &Path) -> InputPaths {
    let source_dirs = discover_source_dirs(packages, cwd);

    let mut globs = Vec::new();
    for dir in &source_dirs {
        let rel = dir.strip_prefix(cwd).unwrap_or(dir);
        if rel.as_os_str().is_empty() {
            continue;
        }
        globs.push(format!("{}/**/*.rs", rel.display()));
    }

    let mut files: Vec<PathBuf> = packages
        .iter()
        .filter_map(|package| package.get("manifest_path").and_then(serde_json::Value::as_str))
        .map(PathBuf::from)
        .map(|manifest| manifest.strip_prefix(cwd).unwrap_or(&manifest).to_path_buf())
        .collect();

    for path in ["Cargo.toml", "Cargo.lock", "rustfmt.toml", ".cargo/config.toml"] {
        let path = PathBuf::from(path);
        if cwd.join(&path).is_file() {
            files.push(path);
        }
    }

    globs.sort();
    globs.dedup();
    files.sort();
    files.dedup();
    InputPaths { globs, files }
}

/// Run `cargo metadata --no-deps` once for the current directory.
fn cargo_metadata() -> Result<serde_json::Value, BoxError> {
    let mut guard = CARGO_METADATA_CACHE.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(metadata) = guard.as_ref() {
        return Ok(metadata.clone());
    }

    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("failed to run `cargo metadata`: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("`cargo metadata` failed: {stderr}").into());
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("failed to parse `cargo metadata` output: {error}"))?;
    *guard = Some(metadata.clone());
    Ok(metadata)
}

/// List Cargo workspace packages.
#[bit_derive::provider_function]
fn packages() -> Result<Vec<String>, BoxError> {
    workspace_package_names(&cargo_metadata()?)
}

/// List a Cargo workspace package's immediate local non-development dependencies.
///
/// `package` names the Cargo workspace package. When `template` is set, every
/// `$` in it is replaced with the dependency package name. For example,
/// `crate[$]` returns matrix block references. Development dependencies are
/// excluded because Cargo does not build them for `cargo build` and permits
/// cycles through them.
#[bit_derive::provider_function]
fn dependencies(package: String, template: Option<String>) -> Result<Vec<BlockRef>, BoxError> {
    Ok(
        workspace_package_dependencies(&cargo_metadata()?, &package, template.as_deref())?
            .into_iter()
            .map(BlockRef::new)
            .collect(),
    )
}

fn workspace_package_names(metadata: &serde_json::Value) -> Result<Vec<String>, BoxError> {
    let mut names: Vec<_> = workspace_packages_by_name(metadata)?.into_keys().collect();
    names.sort();
    Ok(names)
}

fn workspace_packages_by_name(metadata: &serde_json::Value) -> Result<HashMap<String, &serde_json::Value>, BoxError> {
    let members = metadata
        .get("workspace_members")
        .and_then(serde_json::Value::as_array)
        .ok_or("`cargo metadata` output has no workspace_members")?;
    let packages = metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or("`cargo metadata` output has no packages")?;

    let member_ids: HashSet<&str> = members.iter().filter_map(serde_json::Value::as_str).collect();
    Ok(packages
        .iter()
        .filter(|package| {
            package
                .get("id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| member_ids.contains(id))
        })
        .filter_map(|package| {
            package
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(|name| (name.to_owned(), package))
        })
        .collect())
}

fn workspace_package_closure(metadata: &serde_json::Value, selected: &str) -> Result<Vec<String>, BoxError> {
    let packages = workspace_packages_by_name(metadata)?;
    if !packages.contains_key(selected) {
        return Err(format!("Rust package '{selected}' not found in workspace").into());
    }
    let roots: HashMap<PathBuf, String> = packages
        .iter()
        .filter_map(|(name, package)| {
            let manifest = package.get("manifest_path")?.as_str()?;
            Some((Path::new(manifest).parent()?.to_path_buf(), name.clone()))
        })
        .collect();

    let mut found = HashSet::new();
    let mut pending = vec![selected.to_owned()];
    while let Some(name) = pending.pop() {
        if !found.insert(name.clone()) {
            continue;
        }
        for dependency in local_package_dependencies(&packages, &roots, &name, DependencyKinds::All) {
            pending.push(dependency);
        }
    }

    let mut names: Vec<_> = found.into_iter().collect();
    names.sort();
    Ok(names)
}

fn workspace_package_dependencies(
    metadata: &serde_json::Value,
    selected: &str,
    template: Option<&str>,
) -> Result<Vec<String>, BoxError> {
    let packages = workspace_packages_by_name(metadata)?;
    if !packages.contains_key(selected) {
        return Err(format!("Rust package '{selected}' not found in workspace").into());
    }
    if template.is_some_and(|template| !template.contains('$')) {
        return Err("Rust dependency template must contain a `$` placeholder".into());
    }

    let roots: HashMap<PathBuf, String> = packages
        .iter()
        .filter_map(|(name, package)| {
            let manifest = package.get("manifest_path")?.as_str()?;
            Some((Path::new(manifest).parent()?.to_path_buf(), name.clone()))
        })
        .collect();
    let dependencies = local_package_dependencies(&packages, &roots, selected, DependencyKinds::NonDev);
    Ok(match template {
        Some(template) => dependencies
            .into_iter()
            .map(|dependency| template.replace('$', &dependency))
            .collect(),
        None => dependencies,
    })
}

fn local_package_dependencies(
    packages: &HashMap<String, &serde_json::Value>,
    roots: &HashMap<PathBuf, String>,
    selected: &str,
    kinds: DependencyKinds,
) -> Vec<String> {
    let mut names: Vec<_> = packages
        .get(selected)
        .and_then(|package| package.get("dependencies"))
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|dependency| {
            if matches!(kinds, DependencyKinds::NonDev)
                && dependency.get("kind").and_then(serde_json::Value::as_str) == Some("dev")
            {
                return None;
            }
            dependency
                .get("path")
                .and_then(serde_json::Value::as_str)
                .and_then(|path| roots.get(Path::new(path)).cloned())
                .or_else(|| {
                    dependency
                        .get("source")
                        .is_some_and(serde_json::Value::is_null)
                        .then(|| dependency.get("name").and_then(serde_json::Value::as_str))
                        .flatten()
                        .and_then(|name| packages.contains_key(name).then(|| name.to_owned()))
                })
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

#[derive(Clone, Copy)]
enum DependencyKinds {
    All,
    NonDev,
}

/// Directories containing local package sources (the parent of each
/// target's src_path), relative to `cwd`.
fn discover_source_dirs(packages: &[&serde_json::Value], cwd: &Path) -> HashSet<PathBuf> {
    let mut dirs = HashSet::new();

    // Each package has targets with a src_path; collect their parent directories.
    for package in packages {
        // Add the package root (parent of Cargo.toml) for tests/, benches/, examples/.
        if let Some(manifest) = package.get("manifest_path").and_then(|manifest| manifest.as_str())
            && let Some(package_dir) = Path::new(manifest).parent()
        {
            dirs.insert(package_dir.to_path_buf());
        }
        // Add each target's source directory.
        if let Some(targets) = package.get("targets").and_then(|targets| targets.as_array()) {
            for target in targets {
                if let Some(source) = target.get("src_path").and_then(|source| source.as_str())
                    && let Some(parent) = Path::new(source).parent()
                {
                    dirs.insert(parent.to_path_buf());
                }
            }
        }
    }

    // Canonicalize relative to cwd and deduplicate parents that are subdirs of others.
    let canonical: HashSet<PathBuf> = dirs.into_iter().filter_map(|d| d.canonicalize().ok()).collect();

    // Convert back to relative paths.
    let cwd_canon = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    canonical
        .into_iter()
        .map(|d| d.strip_prefix(&cwd_canon).unwrap_or(&d).to_path_buf())
        .collect()
}

/// Rust provider with `build`, `exe`, `test`, `clippy`, and `fmt` resources.
pub struct RustProvider {
    tracker: Arc<Mutex<FileTracker>>,
}

impl RustProvider {
    pub fn new(tracker: Arc<Mutex<FileTracker>>) -> Self {
        Self { tracker }
    }
}

impl Provider for RustProvider {
    fn name(&self) -> &str {
        "rust"
    }

    fn resources(&self) -> Vec<Box<dyn DynResource>> {
        vec![
            Box::new(build::RustBuildResource {
                tracker: Arc::clone(&self.tracker),
            }),
            Box::new(exe::RustExeResource {
                tracker: Arc::clone(&self.tracker),
            }),
            Box::new(test::RustTestResource {
                tracker: Arc::clone(&self.tracker),
            }),
            Box::new(clippy::RustClippyResource {
                tracker: Arc::clone(&self.tracker),
            }),
            Box::new(fmt::RustFmtResource {
                tracker: Arc::clone(&self.tracker),
            }),
            Box::new(fmt::RustFmtCheckResource {
                tracker: Arc::clone(&self.tracker),
            }),
        ]
    }

    fn functions(&self) -> Vec<FuncSignature> {
        vec![__bit_signature_packages(), __bit_signature_dependencies()]
    }

    fn call_function(&self, name: &str, args: &[Value]) -> Result<Value, BoxError> {
        match name {
            "packages" => __bit_call_packages(args),
            "dependencies" => __bit_call_dependencies(args),
            _ => Err(format!("rust provider has no function '{name}'").into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_registration() {
        let tracker = Arc::new(Mutex::new(FileTracker::default()));
        let provider = RustProvider::new(tracker);
        assert_eq!(provider.name(), "rust");
        let resources = provider.resources();
        assert_eq!(resources.len(), 6);
        assert_eq!(resources[0].name(), "build");
        assert_eq!(resources[1].name(), "exe");
        assert_eq!(resources[2].name(), "test");
        assert_eq!(resources[3].name(), "clippy");
        assert_eq!(resources[4].name(), "fmt");
        assert_eq!(resources[5].name(), "fmt-check");
        let functions = provider.functions();
        assert_eq!(functions.len(), 2);
        assert_eq!(functions[0].name, "packages");
        assert_eq!(functions[1].name, "dependencies");
        assert_eq!(
            functions[1].returns,
            crate::value::Type::List(Box::new(crate::value::Type::BlockRef))
        );
    }

    #[test]
    fn workspace_package_names_are_sorted_and_exclude_dependencies() {
        let metadata = serde_json::json!({
            "workspace_members": ["z 0.1.0", "a 0.1.0"],
            "packages": [
                {"id": "dep 1.0.0", "name": "dep"},
                {"id": "z 0.1.0", "name": "z"},
                {"id": "a 0.1.0", "name": "a"}
            ]
        });

        assert_eq!(workspace_package_names(&metadata).unwrap(), vec!["a", "z"]);
    }

    #[test]
    fn input_paths_include_workspace_configuration() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".cargo")).unwrap();
        for path in ["Cargo.toml", "Cargo.lock", "rustfmt.toml", ".cargo/config.toml"] {
            std::fs::write(dir.path().join(path), "").unwrap();
        }

        let paths = discover_input_paths(&[], dir.path());

        for path in ["Cargo.toml", "Cargo.lock", "rustfmt.toml", ".cargo/config.toml"] {
            assert!(paths.files.contains(&PathBuf::from(path)), "missing {path}");
        }
    }

    #[test]
    fn workspace_package_closure_includes_local_dependencies_only() {
        let metadata = serde_json::json!({
            "workspace_members": ["app 0.1.0", "core 0.1.0", "other 0.1.0"],
            "packages": [
                {
                    "id": "app 0.1.0",
                    "name": "app",
                    "manifest_path": "/workspace/app/Cargo.toml",
                    "dependencies": [
                        {"name": "core", "path": "/workspace/core", "source": null},
                        {"name": "serde", "source": "registry+https://example.invalid/index"}
                    ]
                },
                {
                    "id": "core 0.1.0",
                    "name": "core",
                    "manifest_path": "/workspace/core/Cargo.toml",
                    "dependencies": []
                },
                {
                    "id": "other 0.1.0",
                    "name": "other",
                    "manifest_path": "/workspace/other/Cargo.toml",
                    "dependencies": []
                }
            ]
        });

        assert_eq!(
            workspace_package_closure(&metadata, "app").unwrap(),
            vec!["app", "core"]
        );
        assert_eq!(workspace_package_closure(&metadata, "core").unwrap(), vec!["core"]);
    }

    #[test]
    fn workspace_package_dependencies_are_immediate_non_dev_references() {
        let metadata = serde_json::json!({
            "workspace_members": [
                "app 0.1.0",
                "build-helper 0.1.0",
                "core 0.1.0",
                "dev-helper 0.1.0",
                "leaf 0.1.0"
            ],
            "packages": [
                {
                    "id": "app 0.1.0",
                    "name": "app",
                    "manifest_path": "/workspace/app/Cargo.toml",
                    "dependencies": [
                        {"name": "core-alias", "path": "/workspace/core", "source": null, "kind": null},
                        {"name": "build-helper", "path": "/workspace/build-helper", "source": null, "kind": "build"},
                        {"name": "dev-helper", "path": "/workspace/dev-helper", "source": null, "kind": "dev"},
                        {"name": "serde", "source": "registry+https://example.invalid/index"}
                    ]
                },
                {
                    "id": "build-helper 0.1.0",
                    "name": "build-helper",
                    "manifest_path": "/workspace/build-helper/Cargo.toml",
                    "dependencies": []
                },
                {
                    "id": "core 0.1.0",
                    "name": "core",
                    "manifest_path": "/workspace/core/Cargo.toml",
                    "dependencies": [
                        {"name": "leaf", "path": "/workspace/leaf", "source": null}
                    ]
                },
                {
                    "id": "dev-helper 0.1.0",
                    "name": "dev-helper",
                    "manifest_path": "/workspace/dev-helper/Cargo.toml",
                    "dependencies": []
                },
                {
                    "id": "leaf 0.1.0",
                    "name": "leaf",
                    "manifest_path": "/workspace/leaf/Cargo.toml",
                    "dependencies": []
                }
            ]
        });

        assert_eq!(
            workspace_package_dependencies(&metadata, "app", None).unwrap(),
            vec!["build-helper", "core"]
        );
        assert_eq!(
            workspace_package_dependencies(&metadata, "app", Some("crate[$]")).unwrap(),
            vec!["crate[build-helper]", "crate[core]"]
        );
        assert!(workspace_package_dependencies(&metadata, "app", Some("crate")).is_err());
    }

    #[test]
    fn wrapper_script_execs_rustc_with_remap() {
        let script = wrapper_script("/wt/a=/repo");
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.ends_with("exec \"$@\" '--remap-path-prefix=/wt/a=/repo'\n"));
    }

    #[test]
    fn wrapper_script_quotes_single_quotes() {
        let script = wrapper_script("/it's/a=/repo");
        assert!(script.contains("'--remap-path-prefix=/it'\\''s/a=/repo'"), "{script}");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.sh");
        std::fs::write(&path, &script).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let out = Command::new(&path)
            .args(["/bin/sh", "-c", "printf '%s\\n' \"$@\"", "sh", "--crate-name", "x"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "--crate-name\nx\n--remap-path-prefix=/it's/a=/repo\n"
        );
    }

    #[test]
    fn cargo_plain() {
        let env = RustEnv::default();
        assert_eq!(env.cargo("build").display(), "cargo build");
    }

    #[test]
    fn cargo_with_toolchain() {
        let env = RustEnv {
            toolchain: Some("nightly".into()),
            ..Default::default()
        };
        assert_eq!(env.cargo("build").display(), "cargo+nightly build");
    }

    #[test]
    fn cargo_with_target_and_profile() {
        let env = RustEnv {
            target: Some("x86_64-unknown-linux-musl".into()),
            profile: Some("release".into()),
            ..Default::default()
        };
        assert_eq!(
            env.cargo("build").display(),
            "cargo build --target x86_64-unknown-linux-musl --profile release"
        );
    }

    #[test]
    fn cargo_with_features() {
        let env = RustEnv::default();
        let features = RustFeatures {
            features: vec!["serde".into(), "async".into()],
            all_features: false,
        };
        let mut cargo = env.cargo("build");
        cargo.features(&features);
        assert_eq!(cargo.display(), "cargo build --features serde,async");
    }

    #[test]
    fn cargo_with_all_features() {
        let env = RustEnv::default();
        let features = RustFeatures {
            features: vec![],
            all_features: true,
        };
        let mut cargo = env.cargo("build");
        cargo.features(&features);
        assert_eq!(cargo.display(), "cargo build --all-features");
    }

    #[test]
    fn cargo_no_features() {
        let env = RustEnv::default();
        let features = RustFeatures::default();
        let mut cargo = env.cargo("build");
        cargo.features(&features);
        assert_eq!(cargo.display(), "cargo build");
    }
}
