pub mod build;
pub mod exe;
pub mod fmt;
pub mod generate;
pub mod lint;
pub mod scanner;
pub mod test;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::provider::{BoxError, DynResource, FuncSignature, Provider, StructField};
use crate::sha256::SHA256;
use crate::value::{Type, Value};

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
        if let Some(v) = &self.goos {
            cmd.env("GOOS", v);
        }
        if let Some(v) = &self.goarch {
            cmd.env("GOARCH", v);
        }
        if let Some(cgo) = self.cgo {
            cmd.env("CGO_ENABLED", if cgo { "1" } else { "0" });
        }
    }
}

/// Scan Go source files for a package pattern and return hashed inputs.
///
/// `dir`, when set, is the working directory where `go` would be invoked — the
/// scanner walks up from there to locate `go.mod`. Required for blocks that
/// build a separate Go module via the `dir` field; `None` starts from bit's CWD.
pub fn resolve_go_inputs(
    pkg: &str,
    dir: Option<&Path>,
    include_tests: bool,
    tracker: &mut FileTracker,
) -> Result<BTreeMap<String, SHA256>, BoxError> {
    let files: Vec<_> = scanner::scan(pkg, include_tests, dir)?.into_iter().collect();
    tracker.hash_files(&files)
}

/// Fingerprint of the effective Go toolchain and target, for the shared
/// action key. `go env` reports values after the block's `GoEnv` overrides
/// and the ambient environment are applied, so both are captured.
pub fn toolchain_fingerprint(env: &GoEnv, dir: Option<&Path>) -> Result<BTreeMap<String, String>, BoxError> {
    const KEYS: [&str; 8] = [
        "GOVERSION",
        "GOOS",
        "GOARCH",
        "CGO_ENABLED",
        "GOFLAGS",
        "GOEXPERIMENT",
        "GOARM",
        "GOAMD64",
    ];
    let probe_key = format!(
        "go env|{}|{:?}",
        dir.map(|d| d.display().to_string()).unwrap_or_default(),
        env
    );
    let text = super::probe_tool(&probe_key, || {
        let mut cmd = Command::new("go");
        cmd.arg("env").args(KEYS);
        env.apply_to(&mut cmd);
        if let Some(dir) = dir {
            cmd.current_dir(dir);
        }
        cmd
    })?;
    Ok(KEYS
        .iter()
        .zip(text.lines().chain(std::iter::repeat("")))
        .map(|(k, v)| (format!("go.{k}"), v.trim().to_owned()))
        .collect())
}

fn packages(args: &[Value]) -> Result<Value, BoxError> {
    if !(1..=2).contains(&args.len()) {
        return Err(format!("go.packages expects 1 or 2 arguments, got {}", args.len()).into());
    }
    let pattern = args[0].as_str().ok_or("go.packages pattern must be a string")?;
    let dir = args
        .get(1)
        .map(|value| value.as_str().ok_or("go.packages dir must be a string"))
        .transpose()?;

    let mut command = Command::new("go");
    command.args(["list", pattern]);
    if let Some(dir) = dir {
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

fn package_list(output: &[u8]) -> Value {
    let mut packages: Vec<_> = String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    packages.sort();
    packages.dedup();
    Value::List(Type::String, packages.into_iter().map(Value::Str).collect())
}

/// Go provider with `exe`, `build`, and `test` resources.
pub struct GoProvider {
    tracker: Arc<Mutex<FileTracker>>,
}

impl GoProvider {
    pub fn new(tracker: Arc<Mutex<FileTracker>>) -> Self {
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
        vec![FuncSignature {
            name: "packages".into(),
            params: vec![
                (
                    "pattern".into(),
                    StructField {
                        typ: Type::String,
                        default: None,
                        description: Some("Go package pattern (for example, ./...)".into()),
                    },
                ),
                (
                    "dir".into(),
                    StructField {
                        typ: Type::String,
                        default: Some(Value::Str(".".into())),
                        description: Some("Working directory for go list".into()),
                    },
                ),
            ],
            returns: Type::List(Box::new(Type::String)),
        }]
    }

    fn call_function(&self, name: &str, args: &[Value]) -> Result<Value, BoxError> {
        match name {
            "packages" => packages(args),
            _ => Err(format!("go provider has no function '{name}'").into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_registration() {
        let provider = GoProvider::new(Arc::new(Mutex::new(FileTracker::default())));
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
    fn package_list_is_sorted_and_deduplicated() {
        assert_eq!(
            package_list(b"example/z\nexample/a\nexample/z\n"),
            Value::List(
                Type::String,
                vec![Value::Str("example/a".into()), Value::Str("example/z".into())]
            )
        );
    }
}
