use serde::{Deserialize, Serialize};

use crate::output::BlockWriter;
use crate::provider::{ApplyResult, BoxError, PlanAction, PlanResult, ResolvedFile, Resource, ResourceKind};

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
fn base_fmt_command(inputs: &RustFmtInputs) -> CargoCommand {
    let program = if let Some(tc) = &inputs.env.toolchain {
        format!("cargo+{tc}")
    } else {
        "cargo".into()
    };
    let mut cargo = CargoCommand::new(program);
    cargo.arg("fmt");
    if let Some(pkg) = &inputs.package {
        cargo.arg2("-p", pkg);
    }
    cargo.extra_flags(&inputs.flags);
    cargo
}

fn fmt_command(inputs: &RustFmtInputs) -> CargoCommand {
    base_fmt_command(inputs)
}

fn fmt_check_command(inputs: &RustFmtInputs) -> CargoCommand {
    let mut cargo = base_fmt_command(inputs);
    cargo.arg("--check");
    cargo
}

fn resolve(_inputs: &RustFmtInputs) -> Result<Vec<ResolvedFile>, BoxError> {
    let mut files = super::resolve_rust_inputs()?;
    for name in ["rustfmt.toml", ".rustfmt.toml"] {
        let path = std::path::Path::new(name);
        if path.exists() {
            files.push(ResolvedFile::Input(path.to_path_buf()));
        }
    }
    Ok(files)
}

fn plan_action(inputs: &RustFmtInputs, prior_state: Option<&RustFmtState>) -> PlanAction {
    let Some(prior) = prior_state else {
        return PlanAction::Create;
    };
    if prior.package != inputs.package || prior.flags != inputs.flags || prior.env != inputs.env {
        PlanAction::Update
    } else {
        PlanAction::None
    }
}

fn save_state(inputs: &RustFmtInputs) -> RustFmtState {
    RustFmtState {
        package: inputs.package.clone(),
        flags: inputs.flags.clone(),
        env: inputs.env.clone(),
    }
}

// -- rust.fmt (build) ---------------------------------------------------------

pub struct RustFmtResource;

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

    fn resolve(&self, inputs: &RustFmtInputs) -> Result<Vec<ResolvedFile>, BoxError> {
        resolve(inputs)
    }

    fn plan(&self, inputs: &RustFmtInputs, prior_state: Option<&RustFmtState>) -> Result<PlanResult, BoxError> {
        Ok(PlanResult {
            action: plan_action(inputs, prior_state),
            description: fmt_command(inputs).display(),
            reason: None,
        })
    }

    fn apply(
        &self,
        inputs: &RustFmtInputs,
        _prior_state: Option<&RustFmtState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<RustFmtState, RustFmtOutputs>, BoxError> {
        fmt_command(inputs).run(writer)?;
        Ok(ApplyResult {
            outputs: RustFmtOutputs {},
            state: Some(save_state(inputs)),
        })
    }

    fn destroy(&self, _prior_state: &RustFmtState, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }
}

// -- rust.fmt-check (test) ----------------------------------------------------

pub struct RustFmtCheckResource;

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

    fn resolve(&self, inputs: &RustFmtInputs) -> Result<Vec<ResolvedFile>, BoxError> {
        resolve(inputs)
    }

    fn plan(&self, inputs: &RustFmtInputs, prior_state: Option<&RustFmtState>) -> Result<PlanResult, BoxError> {
        Ok(PlanResult {
            action: plan_action(inputs, prior_state),
            description: fmt_check_command(inputs).display(),
            reason: None,
        })
    }

    fn apply(
        &self,
        inputs: &RustFmtInputs,
        _prior_state: Option<&RustFmtState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<RustFmtState, RustFmtCheckOutputs>, BoxError> {
        let passed = fmt_check_command(inputs).run(writer).is_ok();
        Ok(ApplyResult {
            outputs: RustFmtCheckOutputs { passed },
            state: Some(save_state(inputs)),
        })
    }

    fn destroy(&self, _prior_state: &RustFmtState, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_resource_kind_is_build() {
        assert_eq!(Resource::kind(&RustFmtResource), ResourceKind::Build);
    }

    #[test]
    fn fmt_check_resource_kind_is_test() {
        assert_eq!(Resource::kind(&RustFmtCheckResource), ResourceKind::Test);
    }

    #[test]
    fn fmt_plan_create() {
        let inputs = RustFmtInputs {
            package: None,
            flags: vec![],
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustFmtResource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "cargo fmt");
    }

    #[test]
    fn fmt_check_plan_create() {
        let inputs = RustFmtInputs {
            package: None,
            flags: vec![],
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustFmtCheckResource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "cargo fmt --check");
    }

    #[test]
    fn fmt_plan_with_package() {
        let inputs = RustFmtInputs {
            package: Some("my-crate".into()),
            flags: vec![],
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustFmtResource, &inputs, None).unwrap();
        assert_eq!(result.description, "cargo fmt -p my-crate");
    }

    #[test]
    fn fmt_check_plan_with_toolchain() {
        let inputs = RustFmtInputs {
            package: None,
            flags: vec![],
            env: RustEnv {
                toolchain: Some("nightly".into()),
                ..Default::default()
            },
        };
        let result = Resource::plan(&RustFmtCheckResource, &inputs, None).unwrap();
        assert_eq!(result.description, "cargo+nightly fmt --check");
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = RustFmtInputs {
            package: None,
            flags: vec![],
            env: RustEnv::default(),
        };
        let prior = RustFmtState {
            package: None,
            flags: vec![],
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustFmtResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn plan_update_when_flags_changed() {
        let inputs = RustFmtInputs {
            package: None,
            flags: vec!["--config-path".into(), "custom.toml".into()],
            env: RustEnv::default(),
        };
        let prior = RustFmtState {
            package: None,
            flags: vec![],
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustFmtResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }
}
