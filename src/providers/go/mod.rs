pub mod build;
pub mod exe;
pub mod fmt;
pub mod generate;
pub mod lint;
pub mod scanner;
pub mod test;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::provider::{Ambient, BoxError, DynResource, FuncSignature, Provider, VersionCommand};
use crate::sha256::SHA256;
use crate::value::Value;
use scanner::Tests;

/// First-class Go environment variables shared across all go resources.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, bit_derive::Schema)]
pub struct GoEnv {
    /// Target OS
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goos: Option<String>,
    /// Target architecture
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goarch: Option<String>,
    /// Enable cgo
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgo: Option<bool>,
}

impl GoEnv {
    pub fn apply_to(&self, cmd: &mut Command) {
        cmd.envs(self.vars());
    }

    /// The environment variables these overrides set.
    fn vars(&self) -> Vec<(&'static str, String)> {
        let mut vars = Vec::new();
        if let Some(v) = &self.goos {
            vars.push(("GOOS", v.clone()));
        }
        if let Some(v) = &self.goarch {
            vars.push(("GOARCH", v.clone()));
        }
        if let Some(cgo) = self.cgo {
            vars.push(("CGO_ENABLED", if cgo { "1" } else { "0" }.to_owned()));
        }
        vars
    }
}

/// Scan Go source files for a package pattern and return hashed inputs,
/// together with the files matching the block's extra `inputs` globs.
///
/// `dir`, when set, is the working directory where `go` would be invoked — the
/// scanner walks up from there to locate `go.mod`. Required for blocks that
/// build a separate Go module via the `dir` field; `None` starts from bit's CWD.
/// The `inputs` globs are relative to bit's CWD, as in every other block.
pub fn resolve_go_inputs(
    pkg: &str,
    dir: Option<&Path>,
    tests: Tests,
    inputs: &[String],
    tracker: &FileTracker,
) -> Result<BTreeMap<String, SHA256>, BoxError> {
    let files: Vec<_> = scanner::scan(pkg, tests, dir)?.into_iter().collect();
    let mut hashes = tracker.hash_files(&files)?;
    for pattern in inputs {
        hashes.extend(tracker.hash_glob(pattern)?);
    }
    Ok(hashes)
}

/// The effective Go toolchain and target, as `go env` reports them once the
/// block's `GoEnv` overrides and the ambient environment apply. Run in
/// `dir`, the go command honours the module's `toolchain` line, so a
/// toolchain switch behind it is seen.
pub fn toolchain(env: &GoEnv, dir: Option<&str>) -> Ambient {
    let mut command = VersionCommand::new(
        "go",
        &[
            "env",
            "GOVERSION",
            "GOOS",
            "GOARCH",
            "CGO_ENABLED",
            "GOFLAGS",
            "GOEXPERIMENT",
            "GOARM",
            "GOAMD64",
        ],
    )
    .dir(dir);
    for (name, value) in env.vars() {
        command = command.env(name, &value);
    }
    command.ambient()
}

/// List Go packages matching a package pattern.
#[bit_derive::provider_function]
fn packages(pattern: String, dir: Option<String>) -> Result<Vec<String>, BoxError> {
    let mut command = Command::new("go");
    command.args(["list", &pattern]);
    if let Some(dir) = &dir {
        command.current_dir(dir);
    }
    let output = command
        .output()
        .map_err(|error| format!("failed to execute `go list {pattern}`: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "`go list {pattern}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }

    Ok(package_list(&output.stdout))
}

/// List Go modules at or below a directory.
///
/// Returns each `go.mod` directory relative to bit's working directory, skipping
/// `vendor`, `testdata`, and directories starting with `.` or `_`, as `./...` does.
#[bit_derive::provider_function]
fn modules(dir: Option<String>) -> Result<Vec<String>, BoxError> {
    let base = dir.unwrap_or_else(|| ".".to_owned());
    let mut found = Vec::new();
    visit_module_dirs(Path::new(&base), PathBuf::new(), &mut found)?;
    let mut modules: Vec<_> = found
        .into_iter()
        .map(|relative| {
            if relative.as_os_str().is_empty() {
                base.clone()
            } else if base == "." {
                relative.to_string_lossy().into_owned()
            } else {
                Path::new(&base).join(relative).to_string_lossy().into_owned()
            }
        })
        .collect();
    modules.sort();
    Ok(modules)
}

fn visit_module_dirs(base: &Path, relative: PathBuf, found: &mut Vec<PathBuf>) -> Result<(), BoxError> {
    let dir = base.join(&relative);
    if dir.join("go.mod").is_file() {
        found.push(relative.clone());
    }
    let entries = std::fs::read_dir(&dir).map_err(|error| format!("reading {}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !entry.file_type()?.is_dir()
            || name.starts_with('.')
            || name.starts_with('_')
            || name == "vendor"
            || name == "testdata"
        {
            continue;
        }
        visit_module_dirs(base, relative.join(name.as_ref()), found)?;
    }
    Ok(())
}

pub(super) fn package_list(output: &[u8]) -> Vec<String> {
    let mut packages: Vec<_> = String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    packages.sort();
    packages.dedup();
    packages
}

/// Go provider with `exe`, `build`, and `test` resources.
pub struct GoProvider {
    tracker: Arc<FileTracker>,
}

impl GoProvider {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

impl Provider for GoProvider {
    fn name(&self) -> &str {
        "go"
    }

    fn resources(&self) -> Vec<Box<dyn DynResource>> {
        vec![
            Box::new(exe::GoExeResource::new(self.tracker.clone())),
            Box::new(build::GoBuildResource::new(self.tracker.clone())),
            Box::new(generate::GoGenerateResource::new(self.tracker.clone())),
            Box::new(test::GoTestResource::new(self.tracker.clone())),
            Box::new(lint::GoLintResource::new(self.tracker.clone())),
            Box::new(fmt::GoFmtResource::new(self.tracker.clone())),
            Box::new(fmt::GoFmtCheckResource::new(self.tracker.clone())),
        ]
    }

    fn functions(&self) -> Vec<FuncSignature> {
        vec![__bit_signature_packages(), __bit_signature_modules()]
    }

    fn call_function(&self, name: &str, args: &[Value]) -> Result<Value, BoxError> {
        match name {
            "packages" => __bit_call_packages(args),
            "modules" => __bit_call_modules(args),
            _ => Err(format!("go provider has no function '{name}'").into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_registration() {
        let provider = GoProvider::new(Arc::new(FileTracker::default()));
        assert_eq!(provider.name(), "go");
        let resources = provider.resources();
        assert_eq!(resources.len(), 7);
        assert_eq!(resources[0].name(), "exe");
        assert_eq!(resources[1].name(), "build");
        assert_eq!(resources[2].name(), "generate");
        assert_eq!(resources[3].name(), "test");
        assert_eq!(resources[4].name(), "lint");
        assert_eq!(resources[5].name(), "fmt");
        assert_eq!(resources[6].name(), "fmt-l");
    }

    #[test]
    fn modules_lists_nested_modules_and_skips_ignored_dirs() {
        let root = tempfile::tempdir().unwrap();
        for dir in [
            "",
            "svc",
            "lib/inner",
            "svc/vendor/dep",
            "lib/testdata/fixture",
            "_tools",
            ".hidden",
        ] {
            let dir = root.path().join(dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("go.mod"), "module example\n").unwrap();
        }
        std::fs::create_dir_all(root.path().join("plain")).unwrap();

        let base = root.path().to_string_lossy().into_owned();
        assert_eq!(
            modules(Some(base.clone())).unwrap(),
            [base.clone(), format!("{base}/lib/inner"), format!("{base}/svc")]
        );
    }

    #[test]
    fn package_list_is_sorted_and_deduplicated() {
        assert_eq!(
            package_list(b"example/z\nexample/a\nexample/z\n"),
            vec!["example/a", "example/z"]
        );
    }
}
