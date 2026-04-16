use serde::{Deserialize, Serialize};

use crate::output::BlockWriter;
use crate::provider::{
    ApplyResult, BoxError, PlanAction, PlanResult, ResolvedFile, Resource, ResourceKind, ResourceSchema, StructType,
};

use super::{CargoCommand, RustEnv, RustFeatures};

/// Inputs for a `rust.build` block.
#[derive(Debug, Deserialize)]
pub struct RustBuildInputs {
    /// Package to build (maps to `cargo build -p <package>`).
    #[serde(default)]
    pub package: Option<String>,
    /// Extra flags passed to `cargo build`.
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub features: RustFeatures,
    #[serde(flatten)]
    pub env: RustEnv,
}

/// Outputs from a `rust.build` block (none meaningful).
#[derive(Debug, Serialize)]
pub struct RustBuildOutputs {}

/// Persisted state for a `rust.build` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustBuildState {
    pub package: Option<String>,
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub features: RustFeatures,
    #[serde(flatten)]
    pub env: RustEnv,
}

pub struct RustBuildResource;

fn build_command(inputs: &RustBuildInputs) -> CargoCommand {
    let mut cargo = inputs.env.cargo("build");
    if let Some(pkg) = &inputs.package {
        cargo.arg2("-p", pkg);
    }
    cargo.features(&inputs.features).extra_flags(&inputs.flags);
    cargo
}

impl Resource for RustBuildResource {
    type State = RustBuildState;
    type Inputs = RustBuildInputs;
    type Outputs = RustBuildOutputs;

    fn name(&self) -> &str {
        "build"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn schema(&self) -> ResourceSchema {
        let mut fields = vec![
            super::package_field("Package to build (-p flag)"),
            super::flags_field("cargo build"),
        ];
        fields.extend(super::feature_fields());
        fields.extend(super::env_fields());
        ResourceSchema {
            kind: ResourceKind::Build,
            inputs: StructType {
                description: Some("Compile Rust packages".into()),
                fields,
            },
            outputs: StructType {
                description: None,
                fields: vec![],
            },
        }
    }

    fn resolve(&self, _inputs: &RustBuildInputs) -> Result<Vec<ResolvedFile>, BoxError> {
        super::resolve_rust_inputs()
    }

    fn plan(&self, inputs: &RustBuildInputs, prior_state: Option<&RustBuildState>) -> Result<PlanResult, BoxError> {
        let description = build_command(inputs).display();

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
        inputs: &RustBuildInputs,
        _prior_state: Option<&RustBuildState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<RustBuildState, RustBuildOutputs>, BoxError> {
        build_command(inputs).run(writer)?;

        Ok(ApplyResult {
            outputs: RustBuildOutputs {},
            state: Some(RustBuildState {
                package: inputs.package.clone(),
                flags: inputs.flags.clone(),
                features: inputs.features.clone(),
                env: inputs.env.clone(),
            }),
        })
    }

    fn destroy(&self, _prior_state: &RustBuildState, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    fn refresh(&self, prior_state: &RustBuildState) -> Result<ApplyResult<RustBuildState, RustBuildOutputs>, BoxError> {
        Ok(ApplyResult {
            outputs: RustBuildOutputs {},
            state: Some(prior_state.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_kind_is_build() {
        assert_eq!(Resource::kind(&RustBuildResource), ResourceKind::Build);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = RustBuildInputs {
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustBuildResource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "cargo build");
    }

    #[test]
    fn plan_create_with_package() {
        let inputs = RustBuildInputs {
            package: Some("my-crate".into()),
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustBuildResource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "cargo build -p my-crate");
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = RustBuildInputs {
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let prior = RustBuildState {
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustBuildResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn plan_update_when_flags_changed() {
        let inputs = RustBuildInputs {
            package: None,
            flags: vec!["--release".into()],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let prior = RustBuildState {
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustBuildResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn plan_update_when_features_changed() {
        let inputs = RustBuildInputs {
            package: None,
            flags: vec![],
            features: RustFeatures {
                features: vec!["serde".into()],
                all_features: false,
            },
            env: RustEnv::default(),
        };
        let prior = RustBuildState {
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustBuildResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }
}
