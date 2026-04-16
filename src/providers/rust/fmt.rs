use serde::{Deserialize, Serialize};

use crate::output::BlockWriter;
use crate::provider::{
    ApplyResult, BoxError, PlanAction, PlanResult, ResolvedFile, Resource, ResourceKind, ResourceSchema, StructField,
    StructType,
};
use crate::value::Type;

use super::{CargoCommand, RustEnv};

/// Inputs for a `rust.fmt` block.
#[derive(Debug, Deserialize)]
pub struct RustFmtInputs {
    /// Package to check formatting for (maps to `cargo fmt -p <package>`).
    #[serde(default)]
    pub package: Option<String>,
    /// Extra flags passed to `cargo fmt`.
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub env: RustEnv,
}

/// Outputs from a `rust.fmt` block.
#[derive(Debug, Serialize)]
pub struct RustFmtOutputs {
    pub passed: bool,
}

/// Persisted state for a `rust.fmt` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustFmtState {
    pub package: Option<String>,
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub env: RustEnv,
}

pub struct RustFmtResource;

/// Build the cargo fmt command.
/// Note: `cargo fmt` only respects toolchain, not --target/--profile.
fn fmt_command(inputs: &RustFmtInputs) -> CargoCommand {
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
    cargo.arg("--check");
    cargo.extra_flags(&inputs.flags);
    cargo
}

impl Resource for RustFmtResource {
    type State = RustFmtState;
    type Inputs = RustFmtInputs;
    type Outputs = RustFmtOutputs;

    fn name(&self) -> &str {
        "fmt"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Test
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema {
            kind: ResourceKind::Test,
            inputs: StructType {
                description: Some("Check Rust formatting with rustfmt".into()),
                fields: vec![
                    super::package_field("Package to check formatting (-p flag)"),
                    super::flags_field("cargo fmt"),
                    (
                        "toolchain".into(),
                        StructField {
                            typ: Type::Optional(Box::new(Type::String)),
                            default: None,
                            description: Some("Rust toolchain (e.g. \"nightly\")".into()),
                        },
                    ),
                ],
            },
            outputs: super::passed_output(),
        }
    }

    fn resolve(&self, _inputs: &RustFmtInputs) -> Result<Vec<ResolvedFile>, BoxError> {
        let mut files = super::resolve_rust_inputs()?;
        for name in ["rustfmt.toml", ".rustfmt.toml"] {
            let path = std::path::Path::new(name);
            if path.exists() {
                files.push(ResolvedFile::Input(path.to_path_buf()));
            }
        }
        Ok(files)
    }

    fn plan(&self, inputs: &RustFmtInputs, prior_state: Option<&RustFmtState>) -> Result<PlanResult, BoxError> {
        let description = fmt_command(inputs).display();

        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description,
                reason: None,
            });
        };

        let action = if prior.package != inputs.package || prior.flags != inputs.flags || prior.env != inputs.env {
            PlanAction::Update
        } else {
            PlanAction::None
        };

        Ok(PlanResult {
            action,
            description,
            reason: None,
        })
    }

    fn apply(
        &self,
        inputs: &RustFmtInputs,
        _prior_state: Option<&RustFmtState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<RustFmtState, RustFmtOutputs>, BoxError> {
        let passed = fmt_command(inputs).run(writer).is_ok();

        Ok(ApplyResult {
            outputs: RustFmtOutputs { passed },
            state: Some(RustFmtState {
                package: inputs.package.clone(),
                flags: inputs.flags.clone(),
                env: inputs.env.clone(),
            }),
        })
    }

    fn destroy(&self, _prior_state: &RustFmtState, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    fn refresh(&self, prior_state: &RustFmtState) -> Result<ApplyResult<RustFmtState, RustFmtOutputs>, BoxError> {
        Ok(ApplyResult {
            outputs: RustFmtOutputs { passed: true },
            state: Some(prior_state.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_kind_is_test() {
        assert_eq!(Resource::kind(&RustFmtResource), ResourceKind::Test);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = RustFmtInputs {
            package: None,
            flags: vec![],
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustFmtResource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "cargo fmt --check");
    }

    #[test]
    fn plan_create_with_package() {
        let inputs = RustFmtInputs {
            package: Some("my-crate".into()),
            flags: vec![],
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustFmtResource, &inputs, None).unwrap();
        assert_eq!(result.description, "cargo fmt -p my-crate --check");
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

    #[test]
    fn fmt_command_plain() {
        let inputs = RustFmtInputs {
            package: None,
            flags: vec![],
            env: RustEnv::default(),
        };
        assert_eq!(fmt_command(&inputs).display(), "cargo fmt --check");
    }

    #[test]
    fn fmt_command_with_toolchain_and_package() {
        let inputs = RustFmtInputs {
            package: Some("my-crate".into()),
            flags: vec![],
            env: RustEnv {
                toolchain: Some("nightly".into()),
                ..Default::default()
            },
        };
        assert_eq!(fmt_command(&inputs).display(), "cargo+nightly fmt -p my-crate --check");
    }
}
