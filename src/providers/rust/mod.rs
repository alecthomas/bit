pub mod build;
pub mod clippy;
pub mod exe;
pub mod fmt;
pub mod test;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::provider::{BoxError, DynResource, FuncSignature, Provider, ResolvedFile, StructField, StructType};
use crate::value::{Type, Value};

/// Shared Rust environment/config fields flattened into all rust resources.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RustEnv {
    /// Cross-compilation target triple (e.g. "x86_64-unknown-linux-musl").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Build profile ("dev", "release", or a custom profile).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Rust toolchain override (e.g. "nightly", "1.79.0").
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RustFeatures {
    /// List of features to enable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// Enable all features.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all_features: bool,
}

// Schema field helpers to avoid duplication across resources.

fn package_field(desc: &str) -> (String, StructField) {
    (
        "package".into(),
        StructField {
            typ: Type::Optional(Box::new(Type::String)),
            default: None,
            description: Some(desc.into()),
        },
    )
}

fn flags_field(command: &str) -> (String, StructField) {
    (
        "flags".into(),
        StructField {
            typ: Type::Optional(Box::new(Type::List(Box::new(Type::String)))),
            default: None,
            description: Some(format!("Extra flags passed to {command}")),
        },
    )
}

/// Feature and env schema fields shared by build/exe/test/clippy.
fn feature_fields() -> Vec<(String, StructField)> {
    vec![
        (
            "features".into(),
            StructField {
                typ: Type::Optional(Box::new(Type::List(Box::new(Type::String)))),
                default: None,
                description: Some("Features to enable".into()),
            },
        ),
        (
            "all_features".into(),
            StructField {
                typ: Type::Optional(Box::new(Type::Bool)),
                default: None,
                description: Some("Enable all features".into()),
            },
        ),
    ]
}

/// Environment schema fields (target/profile/toolchain) shared by build/exe/test/clippy.
fn env_fields() -> Vec<(String, StructField)> {
    vec![
        (
            "target".into(),
            StructField {
                typ: Type::Optional(Box::new(Type::String)),
                default: None,
                description: Some("Target triple (e.g. \"x86_64-unknown-linux-musl\")".into()),
            },
        ),
        (
            "profile".into(),
            StructField {
                typ: Type::Optional(Box::new(Type::String)),
                default: None,
                description: Some("Build profile (e.g. \"release\")".into()),
            },
        ),
        (
            "toolchain".into(),
            StructField {
                typ: Type::Optional(Box::new(Type::String)),
                default: None,
                description: Some("Rust toolchain (e.g. \"nightly\")".into()),
            },
        ),
    ]
}

/// Schema output for test-kind resources that produce a `passed` bool.
fn passed_output() -> StructType {
    StructType {
        description: None,
        fields: vec![(
            "passed".into(),
            StructField {
                typ: Type::Bool,
                default: None,
                description: Some("Whether the check passed".into()),
            },
        )],
    }
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

    /// Build a `std::process::Command` ready to spawn.
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
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

/// Cached resolved inputs, shared across all rust resources in a run.
static RESOLVED_CACHE: Mutex<Option<Vec<ResolvedFile>>> = Mutex::new(None);

/// Resolve Rust source files for change detection.
///
/// Uses `cargo metadata` to discover local package source directories,
/// then globs for `.rs` files within them. Results are cached so the
/// metadata call only happens once per run.
pub fn resolve_rust_inputs() -> Result<Vec<ResolvedFile>, BoxError> {
    let mut guard = RESOLVED_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cached) = &*guard {
        return Ok(cached.clone());
    }

    let files = resolve_rust_inputs_uncached()?;
    *guard = Some(files.clone());
    Ok(files)
}

fn resolve_rust_inputs_uncached() -> Result<Vec<ResolvedFile>, BoxError> {
    let cwd = std::env::current_dir()?;
    let source_dirs = discover_source_dirs(&cwd)?;

    let mut files = Vec::new();
    for dir in &source_dirs {
        let rel = dir.strip_prefix(&cwd).unwrap_or(dir);
        if rel.as_os_str().is_empty() {
            continue;
        }
        files.push(ResolvedFile::InputGlob(format!("{}/**/*.rs", rel.display())));
    }

    // Include Cargo.toml files for all workspace members.
    for dir in &source_dirs {
        let cargo_toml = dir.join("Cargo.toml");
        if cargo_toml.exists() {
            let rel = cargo_toml.strip_prefix(&cwd).unwrap_or(&cargo_toml);
            files.push(ResolvedFile::Input(rel.to_path_buf()));
        }
    }

    // Root manifest and lock file.
    let cargo_toml = PathBuf::from("Cargo.toml");
    if cargo_toml.exists() {
        files.push(ResolvedFile::Input(cargo_toml));
    }
    let cargo_lock = PathBuf::from("Cargo.lock");
    if cargo_lock.exists() {
        files.push(ResolvedFile::Input(cargo_lock));
    }

    Ok(files)
}

/// Run `cargo metadata --no-deps` and return the set of directories
/// containing local package sources (the parent of each target's src_path).
fn discover_source_dirs(cwd: &Path) -> Result<HashSet<PathBuf>, BoxError> {
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

    let meta: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("failed to parse `cargo metadata` output: {e}"))?;

    let mut dirs = HashSet::new();

    // Each package has targets with a src_path; collect their parent directories.
    if let Some(packages) = meta.get("packages").and_then(|p| p.as_array()) {
        for pkg in packages {
            // Add the package root (parent of Cargo.toml) for tests/, benches/, examples/.
            if let Some(manifest) = pkg.get("manifest_path").and_then(|m| m.as_str())
                && let Some(pkg_dir) = Path::new(manifest).parent()
            {
                dirs.insert(pkg_dir.to_path_buf());
            }
            // Add each target's source directory.
            if let Some(targets) = pkg.get("targets").and_then(|t| t.as_array()) {
                for target in targets {
                    if let Some(src_path) = target.get("src_path").and_then(|s| s.as_str())
                        && let Some(parent) = Path::new(src_path).parent()
                    {
                        dirs.insert(parent.to_path_buf());
                    }
                }
            }
        }
    }

    // Canonicalize relative to cwd and deduplicate parents that are subdirs of others.
    let canonical: HashSet<PathBuf> = dirs.into_iter().filter_map(|d| d.canonicalize().ok()).collect();

    // Convert back to relative paths.
    let cwd_canon = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    Ok(canonical
        .into_iter()
        .map(|d| d.strip_prefix(&cwd_canon).unwrap_or(&d).to_path_buf())
        .collect())
}

/// Rust provider with `build`, `exe`, `test`, `clippy`, and `fmt` resources.
pub struct RustProvider;

impl Provider for RustProvider {
    fn name(&self) -> &str {
        "rust"
    }

    fn resources(&self) -> Vec<Box<dyn DynResource>> {
        vec![
            Box::new(build::RustBuildResource),
            Box::new(exe::RustExeResource),
            Box::new(test::RustTestResource),
            Box::new(clippy::RustClippyResource),
            Box::new(fmt::RustFmtResource),
            Box::new(fmt::RustFmtCheckResource),
        ]
    }

    fn functions(&self) -> Vec<FuncSignature> {
        vec![]
    }

    fn call_function(&self, name: &str, _args: &[Value]) -> Result<Value, BoxError> {
        Err(format!("rust provider has no function '{name}'").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_registration() {
        let provider = RustProvider;
        assert_eq!(provider.name(), "rust");
        let resources = provider.resources();
        assert_eq!(resources.len(), 6);
        assert_eq!(resources[0].name(), "build");
        assert_eq!(resources[1].name(), "exe");
        assert_eq!(resources[2].name(), "test");
        assert_eq!(resources[3].name(), "clippy");
        assert_eq!(resources[4].name(), "fmt");
        assert_eq!(resources[5].name(), "fmt-check");
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
