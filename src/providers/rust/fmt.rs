use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{
    Ambient, ApplyResult, Block, BoxError, CachePolicy, Changes, PlanAction, PlanResult, Resource, ResourceKind,
};
use crate::sha256::SHA256;

use super::{CargoCommand, RustEnv};

/// Format Rust source files
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct RustFmtInputs {
    /// Package to format (-p flag)
    #[serde(default)]
    pub package: Option<String>,
    /// Extra flags passed to cargo fmt
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub env: RustEnv,
}

/// Persisted state shared by `rust.fmt` and `rust.fmt-check`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustFmtState {
    pub package: Option<String>,
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub env: RustEnv,
}

/// Outputs from `rust.fmt` (build, no meaningful outputs).
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct RustFmtOutputs {}

/// Outputs from `rust.fmt-check` (test, pass/fail).
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct RustFmtCheckOutputs {
    /// Whether the check passed
    pub passed: bool,
}

/// Build a cargo fmt CargoCommand. `cargo fmt` only respects toolchain, not --target/--profile.
fn base_fmt_command(inputs: &Block<RustFmtInputs>) -> CargoCommand {
    let program = if let Some(tc) = &inputs.env.toolchain {
        format!("cargo+{tc}")
    } else {
        "cargo".into()
    };
    let mut cargo = CargoCommand::new(program);
    cargo.arg("fmt").in_dir(inputs.dir.as_deref());
    if let Some(pkg) = &inputs.package {
        cargo.arg2("-p", pkg);
    }
    cargo.extra_flags(&inputs.flags);
    cargo
}

fn fmt_command(inputs: &Block<RustFmtInputs>) -> CargoCommand {
    base_fmt_command(inputs)
}

fn fmt_check_command(inputs: &Block<RustFmtInputs>) -> CargoCommand {
    let mut cargo = base_fmt_command(inputs);
    cargo.arg("--check");
    cargo
}

fn resolve(inputs: &Block<RustFmtInputs>, tracker: &FileTracker) -> Result<BTreeMap<String, SHA256>, BoxError> {
    let dir = inputs.dir.as_deref();
    let mut files = super::resolve_rust_inputs(dir, inputs.package.as_deref(), tracker)?;
    files.extend(super::hash_existing(
        &rustfmt_config(dir, inputs.package.as_deref())?,
        tracker,
    )?);
    Ok(files)
}

/// The config files rustfmt may load: `.rustfmt.toml` or `rustfmt.toml` in
/// the directory of a file it formats or any parent, then in the home
/// directory and the user's `rustfmt` config directory, as rustfmt
/// searches. Files inside a package are already its inputs, so the search
/// starts at each package's directory.
fn rustfmt_config(dir: Option<&str>, package: Option<&str>) -> Result<Vec<std::path::PathBuf>, BoxError> {
    const NAMES: [&str; 2] = [".rustfmt.toml", "rustfmt.toml"];
    let mut files = super::config_in_ancestors(&super::package_roots(dir, package)?, &NAMES);
    let user_dirs = [dirs::home_dir(), dirs::config_dir().map(|dir| dir.join("rustfmt"))];
    files.extend(
        user_dirs
            .into_iter()
            .flatten()
            .flat_map(|dir| NAMES.map(|name| dir.join(name))),
    );
    Ok(files)
}

fn plan_action(inputs: &Block<RustFmtInputs>, prior_state: Option<&RustFmtState>) -> PlanAction {
    let Some(prior) = prior_state else {
        return PlanAction::Create;
    };
    if prior.package != inputs.package || prior.flags != inputs.flags || prior.env != inputs.env {
        PlanAction::Update
    } else {
        PlanAction::None
    }
}

fn save_state(inputs: &Block<RustFmtInputs>) -> RustFmtState {
    RustFmtState {
        package: inputs.package.clone(),
        flags: inputs.flags.clone(),
        env: inputs.env.clone(),
    }
}

// -- rust.fmt (build) ---------------------------------------------------------

pub struct RustFmtResource {
    pub tracker: Arc<FileTracker>,
}

impl Resource for RustFmtResource {
    type State = RustFmtState;
    type Inputs = RustFmtInputs;
    type Outputs = RustFmtOutputs;

    fn name(&self) -> &str {
        "fmt"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn ambient(&self, inputs: &Block<RustFmtInputs>) -> Vec<Ambient> {
        super::toolchain(&inputs.env, inputs.dir.as_deref(), &[&["rustfmt", "--version"]])
    }

    fn resolve(&self, inputs: &Block<RustFmtInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let tracker = &*self.tracker;
        resolve(inputs, tracker)
    }

    fn plan(&self, inputs: &Block<RustFmtInputs>, prior_state: Option<&RustFmtState>) -> Result<PlanResult, BoxError> {
        Ok(PlanResult {
            action: plan_action(inputs, prior_state),
            description: fmt_command(inputs).display(),
            reason: None,
        })
    }

    fn apply(
        &self,
        inputs: &Block<RustFmtInputs>,
        _prior_state: Option<&RustFmtState>,
        _changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<RustFmtState, RustFmtOutputs>, BoxError> {
        fmt_command(inputs).run(writer)?;
        Ok(ApplyResult {
            outputs: RustFmtOutputs {},
            state: Some(save_state(inputs)),
        })
    }

    fn destroy(&self, _prior_state: &Block<RustFmtState>, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    fn cache_policy(&self, _inputs: &Block<RustFmtInputs>) -> CachePolicy {
        CachePolicy::Shared { version: 1 }
    }
}

// -- rust.fmt-check (test) ----------------------------------------------------

pub struct RustFmtCheckResource {
    pub tracker: Arc<FileTracker>,
}

impl Resource for RustFmtCheckResource {
    type State = RustFmtState;
    type Inputs = RustFmtInputs;
    type Outputs = RustFmtCheckOutputs;

    fn name(&self) -> &str {
        "fmt-check"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Test
    }

    fn ambient(&self, inputs: &Block<RustFmtInputs>) -> Vec<Ambient> {
        super::toolchain(&inputs.env, inputs.dir.as_deref(), &[&["rustfmt", "--version"]])
    }

    fn resolve(&self, inputs: &Block<RustFmtInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let tracker = &*self.tracker;
        resolve(inputs, tracker)
    }

    fn plan(&self, inputs: &Block<RustFmtInputs>, prior_state: Option<&RustFmtState>) -> Result<PlanResult, BoxError> {
        Ok(PlanResult {
            action: plan_action(inputs, prior_state),
            description: fmt_check_command(inputs).display(),
            reason: None,
        })
    }

    fn apply(
        &self,
        inputs: &Block<RustFmtInputs>,
        _prior_state: Option<&RustFmtState>,
        _changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<RustFmtState, RustFmtCheckOutputs>, BoxError> {
        let passed = fmt_check_command(inputs).run(writer).is_ok();
        Ok(ApplyResult {
            outputs: RustFmtCheckOutputs { passed },
            state: Some(save_state(inputs)),
        })
    }

    fn destroy(&self, _prior_state: &Block<RustFmtState>, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    fn cache_policy(&self, _inputs: &Block<RustFmtInputs>) -> CachePolicy {
        CachePolicy::Shared { version: 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rustfmt_config_is_searched_above_packages_and_in_user_dirs() {
        // bit's own workspace, which the tests run in.
        let files = rustfmt_config(None, Some("bit")).unwrap();
        let root = std::env::current_dir().unwrap();
        for file in [
            root.parent().unwrap().join("rustfmt.toml"),
            dirs::home_dir().unwrap().join(".rustfmt.toml"),
            dirs::config_dir().unwrap().join("rustfmt/rustfmt.toml"),
        ] {
            assert!(files.contains(&file), "missing {}: {files:?}", file.display());
        }
    }

    fn make_fmt_resource() -> RustFmtResource {
        RustFmtResource {
            tracker: Arc::new(FileTracker::default()),
        }
    }

    fn make_fmt_check_resource() -> RustFmtCheckResource {
        RustFmtCheckResource {
            tracker: Arc::new(FileTracker::default()),
        }
    }

    #[test]
    fn fmt_resource_kind_is_build() {
        assert_eq!(Resource::kind(&make_fmt_resource()), ResourceKind::Build);
    }

    #[test]
    fn fmt_check_resource_kind_is_test() {
        assert_eq!(Resource::kind(&make_fmt_check_resource()), ResourceKind::Test);
    }

    #[test]
    fn fmt_plan_create() {
        let inputs = Block::new(
            None,
            RustFmtInputs {
                package: None,
                flags: vec![],
                env: RustEnv::default(),
            },
        );
        let result = Resource::plan(&make_fmt_resource(), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "cargo fmt");
    }

    #[test]
    fn fmt_check_plan_create() {
        let inputs = Block::new(
            None,
            RustFmtInputs {
                package: None,
                flags: vec![],
                env: RustEnv::default(),
            },
        );
        let result = Resource::plan(&make_fmt_check_resource(), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "cargo fmt --check");
    }

    #[test]
    fn fmt_plan_with_package() {
        let inputs = Block::new(
            None,
            RustFmtInputs {
                package: Some("my-crate".into()),
                flags: vec![],
                env: RustEnv::default(),
            },
        );
        let result = Resource::plan(&make_fmt_resource(), &inputs, None).unwrap();
        assert_eq!(result.description, "cargo fmt -p my-crate");
    }

    #[test]
    fn fmt_check_plan_with_toolchain() {
        let inputs = Block::new(
            None,
            RustFmtInputs {
                package: None,
                flags: vec![],
                env: RustEnv {
                    toolchain: Some("nightly".into()),
                    ..Default::default()
                },
            },
        );
        let result = Resource::plan(&make_fmt_check_resource(), &inputs, None).unwrap();
        assert_eq!(result.description, "cargo+nightly fmt --check");
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = Block::new(
            None,
            RustFmtInputs {
                package: None,
                flags: vec![],
                env: RustEnv::default(),
            },
        );
        let prior = RustFmtState {
            package: None,
            flags: vec![],
            env: RustEnv::default(),
        };
        let result = Resource::plan(&make_fmt_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn plan_update_when_flags_changed() {
        let inputs = Block::new(
            None,
            RustFmtInputs {
                package: None,
                flags: vec!["--config-path".into(), "custom.toml".into()],
                env: RustEnv::default(),
            },
        );
        let prior = RustFmtState {
            package: None,
            flags: vec![],
            env: RustEnv::default(),
        };
        let result = Resource::plan(&make_fmt_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }
}
