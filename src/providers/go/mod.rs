pub mod build;
pub mod exe;
pub mod fmt;
pub mod lint;
pub mod scanner;
pub mod test;

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::provider::{BoxError, DynResource, FuncSignature, Provider};
use crate::sha256::SHA256;
use crate::value::Value;

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
pub fn resolve_go_inputs(
    pkg: &str,
    include_tests: bool,
    tracker: &mut FileTracker,
) -> Result<BTreeMap<String, SHA256>, BoxError> {
    let files: Vec<_> = scanner::scan(pkg, include_tests)?.into_iter().collect();
    tracker.hash_files(&files)
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
            Box::new(test::GoTestResource::new(self.tracker.clone())),
            Box::new(lint::GoLintResource::new(self.tracker.clone())),
            Box::new(fmt::GoFmtResource::new(self.tracker.clone())),
            Box::new(fmt::GoFmtCheckResource::new(self.tracker.clone())),
        ]
    }

    fn functions(&self) -> Vec<FuncSignature> {
        vec![]
    }

    fn call_function(&self, name: &str, _args: &[Value]) -> Result<Value, BoxError> {
        Err(format!("go provider has no function '{name}'").into())
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
        assert_eq!(resources.len(), 6);
        assert_eq!(resources[0].name(), "exe");
        assert_eq!(resources[1].name(), "build");
        assert_eq!(resources[2].name(), "test");
        assert_eq!(resources[3].name(), "lint");
        assert_eq!(resources[4].name(), "fmt");
        assert_eq!(resources[5].name(), "fmt-l");
    }
}
