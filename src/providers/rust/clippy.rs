use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::TargetWriter;
use crate::provider::{
    Ambient, ApplyResult, BoxError, CachePolicy, Changes, PlanAction, PlanResult, Resource, ResourceKind, Target,
};
use crate::sha256::SHA256;

use super::{CargoCommand, RustEnv, RustFeatures};

/// Run Clippy linter
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct RustClippyInputs {
    /// Package to lint (-p flag)
    #[serde(default)]
    pub package: Option<String>,
    /// Extra flags passed to cargo clippy
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub features: RustFeatures,
    #[serde(flatten)]
    pub rust_env: RustEnv,
}

/// Outputs from a `rust.clippy` target.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct RustClippyOutputs {
    /// Whether the check passed
    pub passed: bool,
}

/// Persisted state for a `rust.clippy` target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustClippyState {
    pub package: Option<String>,
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub features: RustFeatures,
    #[serde(flatten)]
    pub rust_env: RustEnv,
}

pub struct RustClippyResource {
    pub tracker: Arc<FileTracker>,
}

/// Overrides where clippy starts its search for configuration.
const CLIPPY_CONF_DIR: &str = "CLIPPY_CONF_DIR";

/// The config files clippy may load: `.clippy.toml` or `clippy.toml` from
/// `CLIPPY_CONF_DIR`, or else from each linted package's directory, and
/// every parent directory, as clippy searches.
fn clippy_config(selection: &super::Selection) -> Result<Vec<PathBuf>, BoxError> {
    let starts = match std::env::var_os(CLIPPY_CONF_DIR) {
        Some(conf_dir) => vec![PathBuf::from(conf_dir)],
        None => super::package_roots(selection)?,
    };
    Ok(super::config_in_ancestors(&starts, &[".clippy.toml", "clippy.toml"]))
}

fn clippy_command(inputs: &Target<RustClippyInputs>) -> CargoCommand {
    let mut cargo = inputs.rust_env.cargo("clippy", inputs.dir.as_deref());
    cargo.envs(&inputs.env);
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

    fn ambient(&self, inputs: &Target<RustClippyInputs>) -> Vec<Ambient> {
        let mut ambient = super::build_ambient(
            &inputs.rust_env,
            inputs.dir.as_deref(),
            &inputs.env,
            &[&["cargo", "clippy", "--version"]],
        );
        ambient.push(Ambient::Variable(CLIPPY_CONF_DIR.to_owned()));
        ambient
    }

    fn resolve(&self, inputs: &Target<RustClippyInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let tracker = &*self.tracker;
        let selection = super::Selection {
            dir: inputs.dir.as_deref(),
            env: &inputs.env,
            package: inputs.package.as_deref(),
            flags: &inputs.flags,
        };
        let mut hashes = super::resolve_rust_inputs(&selection, tracker)?;
        hashes.extend(super::hash_existing(&clippy_config(&selection)?, tracker)?);
        Ok(hashes)
    }

    fn plan(
        &self,
        inputs: &Target<RustClippyInputs>,
        prior_state: Option<&RustClippyState>,
    ) -> Result<PlanResult, BoxError> {
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
            || prior.rust_env != inputs.rust_env
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
        inputs: &Target<RustClippyInputs>,
        _prior_state: Option<&RustClippyState>,
        _changes: &Changes,
        writer: &TargetWriter,
    ) -> Result<ApplyResult<RustClippyState, RustClippyOutputs>, BoxError> {
        let passed = clippy_command(inputs).run(writer).is_ok();

        Ok(ApplyResult {
            outputs: RustClippyOutputs { passed },
            state: Some(RustClippyState {
                package: inputs.package.clone(),
                flags: inputs.flags.clone(),
                features: inputs.features.clone(),
                rust_env: inputs.rust_env.clone(),
            }),
        })
    }

    fn destroy(&self, _prior_state: &Target<RustClippyState>, _writer: &TargetWriter) -> Result<(), BoxError> {
        Ok(())
    }

    fn cache_policy(&self, _inputs: &Target<RustClippyInputs>) -> CachePolicy {
        CachePolicy::Shared { version: 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_resource() -> RustClippyResource {
        RustClippyResource {
            tracker: Arc::new(FileTracker::default()),
        }
    }

    #[test]
    fn resource_kind_is_test() {
        assert_eq!(Resource::kind(&make_resource()), ResourceKind::Test);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = Target::new(
            None,
            RustClippyInputs {
                package: None,
                flags: vec![],
                features: RustFeatures::default(),
                rust_env: RustEnv::default(),
            },
        );
        let result = Resource::plan(&make_resource(), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "cargo clippy");
    }

    #[test]
    fn plan_create_with_package() {
        let inputs = Target::new(
            None,
            RustClippyInputs {
                package: Some("my-crate".into()),
                flags: vec![],
                features: RustFeatures::default(),
                rust_env: RustEnv::default(),
            },
        );
        let result = Resource::plan(&make_resource(), &inputs, None).unwrap();
        assert_eq!(result.description, "cargo clippy -p my-crate");
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = Target::new(
            None,
            RustClippyInputs {
                package: None,
                flags: vec![],
                features: RustFeatures::default(),
                rust_env: RustEnv::default(),
            },
        );
        let prior = RustClippyState {
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            rust_env: RustEnv::default(),
        };
        let result = Resource::plan(&make_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn plan_update_when_flags_changed() {
        let inputs = Target::new(
            None,
            RustClippyInputs {
                package: None,
                flags: vec!["--".into(), "-D".into(), "warnings".into()],
                features: RustFeatures::default(),
                rust_env: RustEnv::default(),
            },
        );
        let prior = RustClippyState {
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            rust_env: RustEnv::default(),
        };
        let result = Resource::plan(&make_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }
}
