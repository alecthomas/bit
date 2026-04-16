use serde::{Deserialize, Serialize};

use crate::output::BlockWriter;
use crate::provider::{
    ApplyResult, BoxError, PlanAction, PlanResult, ResolvedFile, Resource, ResourceKind, ResourceSchema, StructType,
};

use super::{CargoCommand, RustEnv, RustFeatures};

/// Inputs for a `rust.clippy` block.
#[derive(Debug, Deserialize)]
pub struct RustClippyInputs {
    /// Package to lint (maps to `cargo clippy -p <package>`).
    #[serde(default)]
    pub package: Option<String>,
    /// Extra flags passed to `cargo clippy`.
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub features: RustFeatures,
    #[serde(flatten)]
    pub env: RustEnv,
}

/// Outputs from a `rust.clippy` block.
#[derive(Debug, Serialize)]
pub struct RustClippyOutputs {
    pub passed: bool,
}

/// Persisted state for a `rust.clippy` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustClippyState {
    pub package: Option<String>,
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub features: RustFeatures,
    #[serde(flatten)]
    pub env: RustEnv,
}

pub struct RustClippyResource;

fn clippy_command(inputs: &RustClippyInputs) -> CargoCommand {
    let mut cargo = inputs.env.cargo("clippy");
    if let Some(pkg) = &inputs.package {
        cargo.arg2("-p", pkg);
    }
    cargo.features(&inputs.features).extra_flags(&inputs.flags);
    cargo
}

impl Resource for RustClippyResource {
    type State = RustClippyState;
    type Inputs = RustClippyInputs;
    type Outputs = RustClippyOutputs;

    fn name(&self) -> &str {
        "clippy"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Test
    }

    fn schema(&self) -> ResourceSchema {
        let mut fields = vec![
            super::package_field("Package to lint (-p flag)"),
            super::flags_field("cargo clippy"),
        ];
        fields.extend(super::feature_fields());
        fields.extend(super::env_fields());
        ResourceSchema {
            kind: ResourceKind::Test,
            inputs: StructType {
                description: Some("Run Clippy linter".into()),
                fields,
            },
            outputs: super::passed_output(),
        }
    }

    fn resolve(&self, _inputs: &RustClippyInputs) -> Result<Vec<ResolvedFile>, BoxError> {
        super::resolve_rust_inputs()
    }

    fn plan(&self, inputs: &RustClippyInputs, prior_state: Option<&RustClippyState>) -> Result<PlanResult, BoxError> {
        let description = clippy_command(inputs).display();

        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description,
                reason: None,
            });
        };

        let action = if prior.package != inputs.package
            || prior.flags != inputs.flags
            || prior.features != inputs.features
            || prior.env != inputs.env
        {
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
        inputs: &RustClippyInputs,
        _prior_state: Option<&RustClippyState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<RustClippyState, RustClippyOutputs>, BoxError> {
        let passed = clippy_command(inputs).run(writer).is_ok();

        Ok(ApplyResult {
            outputs: RustClippyOutputs { passed },
            state: Some(RustClippyState {
                package: inputs.package.clone(),
                flags: inputs.flags.clone(),
                features: inputs.features.clone(),
                env: inputs.env.clone(),
            }),
        })
    }

    fn destroy(&self, _prior_state: &RustClippyState, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    fn refresh(
        &self,
        prior_state: &RustClippyState,
    ) -> Result<ApplyResult<RustClippyState, RustClippyOutputs>, BoxError> {
        Ok(ApplyResult {
            outputs: RustClippyOutputs { passed: true },
            state: Some(prior_state.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_kind_is_test() {
        assert_eq!(Resource::kind(&RustClippyResource), ResourceKind::Test);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = RustClippyInputs {
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustClippyResource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "cargo clippy");
    }

    #[test]
    fn plan_create_with_package() {
        let inputs = RustClippyInputs {
            package: Some("my-crate".into()),
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustClippyResource, &inputs, None).unwrap();
        assert_eq!(result.description, "cargo clippy -p my-crate");
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = RustClippyInputs {
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let prior = RustClippyState {
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustClippyResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn plan_update_when_flags_changed() {
        let inputs = RustClippyInputs {
            package: None,
            flags: vec!["--".into(), "-D".into(), "warnings".into()],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let prior = RustClippyState {
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustClippyResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }
}
