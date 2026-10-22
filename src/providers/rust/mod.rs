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
use crate::provider::{Ambient, BoxError, DynResource, FuncSignature, Provider, VersionCommand};
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
    /// Start building a cargo command for the given subcommand, run in `dir`.
    pub fn cargo(&self, subcommand: &str, dir: Option<&str>) -> CargoCommand {
        let program = if let Some(tc) = &self.toolchain {
            format!("cargo+{tc}")
        } else {
            "cargo".into()
        };
        let mut cargo = CargoCommand::new(program);
        cargo.arg(subcommand).in_dir(dir);
        if let Some(t) = &self.target {
            cargo.arg2("--target", t);
        }
        if let Some(p) = &self.profile {
            cargo.arg2("--profile", p);
        }
        cargo
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
    dir: Option<String>,
}

impl CargoCommand {
    /// Create a new CargoCommand with the given program name.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            dir: None,
        }
    }

    /// Run the command in `dir` rather than bit's working directory.
    pub fn in_dir(&mut self, dir: Option<&str>) -> &mut Self {
        self.dir = dir.map(str::to_owned);
        self
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
        crate::providers::display_command(&self.program, &self.args)
    }

    /// Build a `std::process::Command` ready to spawn. In a linked worktree,
    /// workspace crates are compiled through a wrapper script that remaps
    /// source paths (see [`remap_wrapper`]).
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        if let Some(dir) = &self.dir {
            cmd.current_dir(dir);
        }
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

/// `rustc -vV` and `extra` version commands for the selected toolchain, run
/// in `dir`. Through rustup, they report the toolchain that `toolchain` or
/// the `rust-toolchain.toml` cargo would find there selects.
pub fn toolchain(env: &RustEnv, dir: Option<&str>, extra: &[&[&str]]) -> Vec<Ambient> {
    let version = |command: &[&str]| {
        let mut command = VersionCommand::new(command[0], &command[1..]).dir(dir);
        if let Some(tc) = &env.toolchain {
            command = command.env("RUSTUP_TOOLCHAIN", tc);
        }
        command.ambient()
    };
    let mut ambient = vec![version(&["rustc", "-vV"])];
    ambient.extend(extra.iter().map(|command| version(command)));
    ambient
}

/// [`toolchain`] plus the variables cargo reads that change compiler
/// output, for the resources that compile. No version command reports
/// them, and without them a binary built with different flags in another
/// worktree could be restored from the shared cache.
pub fn build_ambient(env: &RustEnv, dir: Option<&str>, extra: &[&[&str]]) -> Vec<Ambient> {
    const BUILD_VARIABLES: [&str; 4] = [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
        "CARGO_BUILD_TARGET",
    ];
    let mut ambient = toolchain(env, dir, extra);
    ambient.extend(BUILD_VARIABLES.iter().map(|var| Ambient::Variable((*var).to_owned())));
    ambient
}

/// Package directories, individual files, and directories to leave out,
/// discovered by `cargo metadata`. The cache avoids re-running the
/// expensive metadata call on every resolve.
#[derive(Clone)]
struct InputPaths {
    /// Directories of the local packages, whose files are listed afresh on
    /// every resolve so a new file is seen.
    roots: Vec<PathBuf>,
    /// The target directory and every local package directory. A package
    /// nested in another's directory is an input only through its own root.
    excluded: Vec<PathBuf>,
    files: Vec<PathBuf>,
}

#[derive(Clone)]
struct DiscoveredPaths {
    workspace: InputPaths,
    packages: HashMap<String, InputPaths>,
    /// Configuration every package of the workspace is built with, hashed
    /// when it exists.
    config: Vec<PathBuf>,
    target_dir: PathBuf,
}

/// Discovered paths and `cargo metadata` output, by the `dir` cargo runs in.
/// Blocks in different directories can belong to different workspaces.
static DISCOVERED_CACHE: LazyLock<Mutex<HashMap<String, DiscoveredPaths>>> = LazyLock::new(Default::default);
static CARGO_METADATA_CACHE: LazyLock<Mutex<HashMap<String, serde_json::Value>>> = LazyLock::new(Default::default);

/// The cache key for `dir`, where `None` is bit's working directory.
fn dir_key(dir: Option<&str>) -> String {
    dir.unwrap_or(".").to_owned()
}

/// Discover once per process and `dir`, and return a copy so the lock is
/// released before any hashing.
fn discovered(dir: Option<&str>) -> Result<DiscoveredPaths, BoxError> {
    let mut guard = DISCOVERED_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(paths) = guard.get(&dir_key(dir)) {
        return Ok(paths.clone());
    }
    let paths = discover_paths(dir)?;
    guard.insert(dir_key(dir), paths.clone());
    Ok(paths)
}

/// Cargo's target directory for the workspace in `dir`, honouring
/// `CARGO_TARGET_DIR` and config.
pub fn target_directory(dir: Option<&str>) -> Result<PathBuf, BoxError> {
    Ok(discovered(dir)?.target_dir)
}

/// Resolve Rust source files for change detection.
///
/// Uses `cargo metadata` in `dir` to discover the local packages, then
/// hashes every file in their directories that Git does not ignore. Builds
/// read more than `.rs` files: `include_str!` and `include_bytes!` data,
/// `#[path]` modules, files a build script reads, and test fixtures. When
/// `package` is set, only that package and its transitive local dependencies
/// are included. The metadata discovery is cached so the call only happens
/// once per run and `dir`.
pub fn resolve_rust_inputs(
    dir: Option<&str>,
    package: Option<&str>,
    tracker: &FileTracker,
) -> Result<BTreeMap<String, SHA256>, BoxError> {
    let discovered = discovered(dir)?;
    let paths = match package {
        Some(package) => discovered
            .packages
            .get(package)
            .ok_or_else(|| format!("Rust package '{package}' not found in workspace"))?,
        None => &discovered.workspace,
    };

    let mut result = BTreeMap::new();
    for root in &paths.roots {
        result.extend(tracker.hash_files(&package_files(root, &paths.excluded))?);
    }
    for path in paths.files.iter().chain(&discovered.config) {
        if path.is_file() {
            let key = path.display().to_string();
            result.insert(key, tracker.hash_file(path)?);
        }
    }
    Ok(result)
}

/// Discover source globs, individual files, and the target directory from
/// `cargo metadata` in `dir`. The paths are as bit's process sees them.
fn discover_paths(dir: Option<&str>) -> Result<DiscoveredPaths, BoxError> {
    let cwd = std::env::current_dir()?.join(dir.unwrap_or(""));
    let meta = cargo_metadata(dir)?;
    let target_dir = meta
        .get("target_directory")
        .and_then(|t| t.as_str())
        .map(PathBuf::from)
        .ok_or("`cargo metadata` output has no target_directory")?;
    let local = local_packages(&meta);
    let workspace_values: Vec<_> = local.values().copied().collect();
    let mut excluded: Vec<PathBuf> = workspace_values
        .iter()
        .filter_map(|package| package_root(package))
        .collect();
    excluded.push(target_dir.clone());
    let workspace = discover_input_paths(&workspace_values, &cwd, dir, &excluded);
    let mut packages = HashMap::new();
    for (name, member) in workspace_packages_by_name(&meta)? {
        let values = local_closure(&meta, &local, member)?;
        packages.insert(name, discover_input_paths(&values, &cwd, dir, &excluded));
    }

    // The workspace root holds the lockfile and the `[patch]`, `[profile]`
    // and `[workspace]` tables every member is built with, even when cargo
    // runs in a member's directory.
    let workspace_root = meta
        .get("workspace_root")
        .and_then(|root| root.as_str())
        .map(PathBuf::from)
        .ok_or("`cargo metadata` output has no workspace_root")?;
    let mut config = vec![workspace_root.join("Cargo.toml"), workspace_root.join("Cargo.lock")];
    let in_dir = |path: &str| PathBuf::from(crate::provider::in_dir(dir, path));
    config.extend(["rustfmt.toml", ".cargo/config.toml"].map(in_dir));

    Ok(DiscoveredPaths {
        workspace,
        packages,
        config,
        target_dir,
    })
}

/// The directory of a package: where its manifest is.
fn package_root(package: &serde_json::Value) -> Option<PathBuf> {
    let manifest = package.get("manifest_path")?.as_str()?;
    Some(Path::new(manifest).parent()?.to_path_buf())
}

/// The input paths of `packages`, found relative to `cwd`, where cargo runs.
/// Files are returned as bit's process sees them: prefixed by `dir`.
fn discover_input_paths(
    packages: &[&serde_json::Value],
    cwd: &Path,
    dir: Option<&str>,
    excluded: &[PathBuf],
) -> InputPaths {
    let in_dir = |path: &Path| PathBuf::from(crate::provider::in_dir(dir, &path.to_string_lossy()));
    let mut roots: Vec<PathBuf> = packages.iter().filter_map(|package| package_root(package)).collect();
    roots.extend(packages.iter().flat_map(|package| outside_target_dirs(package)));

    let mut files: Vec<PathBuf> = packages
        .iter()
        .filter_map(|package| package.get("manifest_path").and_then(serde_json::Value::as_str))
        .map(PathBuf::from)
        .map(|manifest| in_dir(manifest.strip_prefix(cwd).unwrap_or(&manifest)))
        .collect();

    roots.sort();
    roots.dedup();
    files.sort();
    files.dedup();
    InputPaths {
        roots,
        excluded: excluded.to_vec(),
        files,
    }
}

/// Every file in the package directory `root` that Git does not ignore,
/// except those under `excluded` other than `root` itself. Outside a Git
/// repository, such as for a path dependency elsewhere, every file but
/// those in `.git` directories.
fn package_files(root: &Path, excluded: &[PathBuf]) -> Vec<PathBuf> {
    let files = git_files(root).unwrap_or_else(|| {
        let mut files = Vec::new();
        walk_files(root, &mut files);
        files
    });
    files
        .into_iter()
        .filter(|file| !excluded.iter().any(|dir| dir != root && file.starts_with(dir)))
        .collect()
}

/// The files under `root` that Git tracks or would track, or `None` when
/// `root` is not in a Git work tree.
fn git_files(root: &Path) -> Option<Vec<PathBuf>> {
    let output = crate::git::command()
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            ".",
        ])
        .current_dir(root)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| root.join(String::from_utf8_lossy(path).as_ref()))
            // A tracked file deleted from the work tree is still listed.
            .filter(|path| path.is_file())
            .collect(),
    )
}

fn walk_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() && entry.file_name() != ".git" => walk_files(&path, files),
            Ok(_) if path.is_file() => files.push(path),
            _ => {}
        }
    }
}

/// Run `cargo metadata` once per process for `dir`.
///
/// The full form resolves the dependency graph, so it reports every local
/// package cargo builds: workspace members, path dependencies outside the
/// workspace, and `[patch]` and `[replace]` paths. It is run `--locked` so
/// resolving never rewrites `Cargo.lock`, and `--offline` so it never hits
/// the network. Until the lockfile and the registry crates exist, which the
/// first build provides, only the workspace members can be reported.
fn cargo_metadata(dir: Option<&str>) -> Result<serde_json::Value, BoxError> {
    let mut guard = CARGO_METADATA_CACHE.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(metadata) = guard.get(&dir_key(dir)) {
        return Ok(metadata.clone());
    }
    let metadata = match run_cargo_metadata(dir, &["--locked", "--offline"]) {
        Ok(metadata) => metadata,
        Err(_) => run_cargo_metadata(dir, &["--no-deps"])?,
    };
    guard.insert(dir_key(dir), metadata.clone());
    Ok(metadata)
}

fn run_cargo_metadata(dir: Option<&str>, flags: &[&str]) -> Result<serde_json::Value, BoxError> {
    let mut command = Command::new("cargo");
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    let output = command
        .args(["metadata", "--format-version", "1"])
        .args(flags)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("failed to run `cargo metadata`: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("`cargo metadata` failed: {stderr}").into());
    }
    Ok(serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("failed to parse `cargo metadata` output: {error}"))?)
}

/// The packages cargo builds from local sources, by package id: those with
/// no registry or Git `source`.
fn local_packages(metadata: &serde_json::Value) -> HashMap<&str, &serde_json::Value> {
    metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|package| package.get("source").is_none_or(serde_json::Value::is_null))
        .filter_map(|package| Some((package.get("id")?.as_str()?, package)))
        .collect()
}

/// `member` and the local packages it depends on, transitively, following
/// cargo's resolved graph. A registry crate can depend on a local package
/// through `[patch]`, so the walk passes through every package. Without a
/// resolved graph, it falls back to the workspace members `member` names.
fn local_closure<'a>(
    metadata: &'a serde_json::Value,
    local: &HashMap<&str, &'a serde_json::Value>,
    member: &'a serde_json::Value,
) -> Result<Vec<&'a serde_json::Value>, BoxError> {
    let Some(nodes) = metadata.pointer("/resolve/nodes").and_then(serde_json::Value::as_array) else {
        let name = member
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let members = workspace_packages_by_name(metadata)?;
        return Ok(workspace_package_closure(metadata, name)?
            .iter()
            .filter_map(|name| members.get(name).copied())
            .collect());
    };
    let dependencies: HashMap<&str, Vec<&str>> = nodes
        .iter()
        .filter_map(|node| {
            let id = node.get("id")?.as_str()?;
            let deps = node.get("dependencies")?.as_array()?;
            Some((id, deps.iter().filter_map(serde_json::Value::as_str).collect()))
        })
        .collect();
    let start = member.get("id").and_then(serde_json::Value::as_str).unwrap_or_default();
    let mut seen = HashSet::new();
    let mut pending = vec![start];
    let mut closure = Vec::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        if let Some(package) = local.get(id) {
            closure.push(*package);
        }
        pending.extend(dependencies.get(id).into_iter().flatten());
    }
    Ok(closure)
}

/// List Cargo workspace packages.
#[bit_derive::provider_function]
fn packages() -> Result<Vec<String>, BoxError> {
    workspace_package_names(&cargo_metadata(None)?)
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
        workspace_package_dependencies(&cargo_metadata(None)?, &package, template.as_deref())?
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
            .map(|dependency| template.replace('$', &Value::Str(dependency).to_literal()))
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

/// The directory of each of `package`'s targets that lies outside the
/// package directory, which a `[[bin]]` or `[lib]` `path` can name.
fn outside_target_dirs(package: &serde_json::Value) -> Vec<PathBuf> {
    let Some(root) = package_root(package) else {
        return vec![];
    };
    package
        .get("targets")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|target| Some(Path::new(target.get("src_path")?.as_str()?).parent()?.to_path_buf()))
        .filter(|dir| !dir.starts_with(&root))
        .collect()
}

/// Rust provider with `build`, `exe`, `test`, `clippy`, and `fmt` resources.
pub struct RustProvider {
    tracker: Arc<FileTracker>,
}

impl RustProvider {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
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

    fn write(root: &Path, files: &[(&str, &str)]) {
        for (path, content) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
    }

    /// A workspace in `ws/` whose `app` depends on `lib` outside it, and on
    /// `patched` through `[patch]`, with a lockfile so cargo can resolve.
    fn workspace_with_path_dependencies() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            &[
                (
                    "ws/Cargo.toml",
                    "[workspace]\nmembers = [\"app\", \"other\"]\nresolver = \"2\"\n\
                     [patch.crates-io]\npatched = { path = \"../patched\" }\n",
                ),
                (
                    "ws/app/Cargo.toml",
                    "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
                     [dependencies]\nlib = { path = \"../../lib\" }\npatched = \"0.1\"\n",
                ),
                ("ws/app/src/main.rs", "fn main() {}\n"),
                (
                    "ws/other/Cargo.toml",
                    "[package]\nname = \"other\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
                ),
                ("ws/other/src/lib.rs", ""),
                (
                    "lib/Cargo.toml",
                    "[package]\nname = \"lib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
                ),
                ("lib/src/lib.rs", ""),
                (
                    "patched/Cargo.toml",
                    "[package]\nname = \"patched\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
                ),
                ("patched/src/lib.rs", ""),
            ],
        );
        let status = Command::new("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(dir.path().join("ws"))
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        dir
    }

    #[test]
    fn inputs_are_every_package_file_git_does_not_ignore() {
        let dir = workspace_with_path_dependencies();
        write(
            dir.path(),
            &[
                ("ws/.gitignore", "ignored.txt\n"),
                ("ws/app/build.rs", "fn main() {}\n"),
                ("ws/app/data/fixture.json", "{}"),
                ("ws/app/ignored.txt", ""),
                ("ws/target/debug/out.rs", ""),
                ("lib/data.bin", ""),
            ],
        );
        let ws = dir.path().join("ws");
        crate::git::command()
            .arg("init")
            .arg("-q")
            .current_dir(&ws)
            .status()
            .unwrap();
        let ws = ws.to_string_lossy().into_owned();
        let tracker = FileTracker::default();
        let names: Vec<String> = resolve_rust_inputs(Some(&ws), Some("app"), &tracker)
            .unwrap()
            .into_keys()
            .map(|path| {
                let path = Path::new(&path);
                path.strip_prefix(dir.path().canonicalize().unwrap())
                    .or_else(|_| path.strip_prefix(dir.path()))
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        for expected in [
            "ws/app/build.rs",
            "ws/app/data/fixture.json",
            "ws/app/src/main.rs",
            "lib/data.bin",
        ] {
            assert!(
                names.iter().any(|name| name.ends_with(expected)),
                "missing {expected}: {names:?}"
            );
        }
        for unexpected in ["ignored.txt", "target/debug/out.rs", "other/src/lib.rs"] {
            assert!(
                !names.iter().any(|name| name.ends_with(unexpected)),
                "has {unexpected}: {names:?}"
            );
        }
    }

    #[test]
    fn discovery_follows_path_dependencies_and_patches_outside_the_workspace() {
        let dir = workspace_with_path_dependencies();
        let ws = dir.path().join("ws").to_string_lossy().into_owned();
        let discovered = discover_paths(Some(&ws)).unwrap();
        let has = |paths: &InputPaths, file: &str| paths.files.iter().any(|path| path.ends_with(file));

        let app = &discovered.packages["app"];
        assert!(has(app, "lib/Cargo.toml"), "{:?}", app.files);
        assert!(has(app, "patched/Cargo.toml"), "{:?}", app.files);
        assert!(!has(app, "other/Cargo.toml"), "{:?}", app.files);
        assert!(has(&discovered.workspace, "other/Cargo.toml"));
        assert!(has(&discovered.workspace, "lib/Cargo.toml"));
    }

    #[test]
    fn provider_registration() {
        let tracker = Arc::new(FileTracker::default());
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
    fn config_comes_from_the_workspace_root_when_run_in_a_member() {
        let dir = workspace_with_path_dependencies();
        let app = dir.path().join("ws/app").to_string_lossy().into_owned();
        let config = discover_paths(Some(&app)).unwrap().config;
        for file in ["ws/Cargo.toml", "ws/Cargo.lock"] {
            assert!(
                config.iter().any(|path| path.ends_with(file)),
                "missing {file}: {config:?}"
            );
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
            vec![r#"crate["build-helper"]"#, r#"crate["core"]"#]
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
        assert_eq!(env.cargo("build", None).display(), "cargo build");
        assert_eq!(env.cargo("build", None).command().get_current_dir(), None);
    }

    #[test]
    fn cargo_runs_in_dir() {
        let cargo = RustEnv::default().cargo("build", Some("crates/app"));
        assert_eq!(cargo.command().get_current_dir(), Some(Path::new("crates/app")));
    }

    #[test]
    fn cargo_with_toolchain() {
        let env = RustEnv {
            toolchain: Some("nightly".into()),
            ..Default::default()
        };
        assert_eq!(env.cargo("build", None).display(), "cargo+nightly build");
    }

    #[test]
    fn cargo_with_target_and_profile() {
        let env = RustEnv {
            target: Some("x86_64-unknown-linux-musl".into()),
            profile: Some("release".into()),
            ..Default::default()
        };
        assert_eq!(
            env.cargo("build", None).display(),
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
        let mut cargo = env.cargo("build", None);
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
        let mut cargo = env.cargo("build", None);
        cargo.features(&features);
        assert_eq!(cargo.display(), "cargo build --all-features");
    }

    #[test]
    fn cargo_no_features() {
        let env = RustEnv::default();
        let features = RustFeatures::default();
        let mut cargo = env.cargo("build", None);
        cargo.features(&features);
        assert_eq!(cargo.display(), "cargo build");
    }
}
