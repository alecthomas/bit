use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{
    Ambient, ApplyResult, Block, BoxError, CachePolicy, Changes, PlanAction, PlanResult, Resource, ResourceKind,
};
use crate::sha256::SHA256;

use super::{run_pnpm, workspace, workspace_dir};

fn default_frozen() -> bool {
    true
}

/// Install the dependencies of the pnpm workspace in `dir`.
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct PnpmInstallInputs {
    /// Pass `--frozen-lockfile` (reproducible installs, default `true`)
    #[serde(default = "default_frozen")]
    pub frozen: bool,
}

/// Outputs from a `pnpm.install` block.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct PnpmInstallOutputs {
    /// Absolute path to the installed `node_modules` directory
    pub path: String,
}

/// Persisted state for a `pnpm.install` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnpmInstallState {
    pub frozen: bool,
    /// Absolute paths of every directory that contains a `node_modules/`.
    /// Used by `destroy` to remove them all.
    pub node_modules_dirs: Vec<String>,
}

pub struct PnpmInstallResource {
    pub(super) tracker: Arc<FileTracker>,
}

impl Resource for PnpmInstallResource {
    type State = PnpmInstallState;
    type Inputs = PnpmInstallInputs;
    type Outputs = PnpmInstallOutputs;

    fn name(&self) -> &str {
        "install"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn ambient(&self, inputs: &Block<PnpmInstallInputs>) -> Vec<Ambient> {
        super::ambient(workspace_dir(inputs))
    }

    fn resolve(&self, inputs: &Block<PnpmInstallInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let tracker = &*self.tracker;
        workspace::with_workspace(Path::new(workspace_dir(inputs)), |ws| {
            let mut files = BTreeMap::new();
            files.insert(
                ws.root_package_json.to_string_lossy().into_owned(),
                tracker.hash_file(&ws.root_package_json)?,
            );
            if let Some(lf) = &ws.lockfile {
                files.insert(lf.to_string_lossy().into_owned(), tracker.hash_file(lf)?);
            }
            if let Some(y) = &ws.workspace_yaml {
                files.insert(y.to_string_lossy().into_owned(), tracker.hash_file(y)?);
            }
            let manifests: Vec<PathBuf> = ws.projects.iter().map(|p| p.dir.join("package.json")).collect();
            files.extend(tracker.hash_files(&manifests)?);
            Ok(files)
        })
    }

    fn plan(
        &self,
        inputs: &Block<PnpmInstallInputs>,
        prior_state: Option<&PnpmInstallState>,
    ) -> Result<PlanResult, BoxError> {
        let description = if inputs.frozen {
            "pnpm install --frozen-lockfile".to_owned()
        } else {
            "pnpm install".to_owned()
        };

        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description,
                reason: None,
            });
        };
        let action = if prior.frozen != inputs.frozen {
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
        inputs: &Block<PnpmInstallInputs>,
        _prior_state: Option<&PnpmInstallState>,
        _changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<PnpmInstallState, PnpmInstallOutputs>, BoxError> {
        let mut args = vec!["install".to_owned()];
        if inputs.frozen {
            args.push("--frozen-lockfile".to_owned());
        }
        run_pnpm(&args, Some(workspace_dir(inputs)), writer)?;

        let (node_modules_path, node_modules_dirs) =
            workspace::with_workspace(Path::new(workspace_dir(inputs)), |ws| {
                let mut dirs: Vec<String> = vec![ws.root.to_string_lossy().into_owned()];
                for project in &ws.projects {
                    if project.dir != ws.root {
                        dirs.push(project.dir.to_string_lossy().into_owned());
                    }
                }
                let nm = ws.root.join("node_modules").to_string_lossy().into_owned();
                Ok((nm, dirs))
            })?;

        Ok(ApplyResult {
            outputs: PnpmInstallOutputs {
                path: node_modules_path,
            },
            state: Some(PnpmInstallState {
                frozen: inputs.frozen,
                node_modules_dirs,
            }),
        })
    }

    fn destroy(&self, prior_state: &Block<PnpmInstallState>, writer: &BlockWriter) -> Result<(), BoxError> {
        for dir in &prior_state.node_modules_dirs {
            let nm: PathBuf = Path::new(dir).join("node_modules");
            crate::providers::remove_path(&nm, writer)?;
        }
        Ok(())
    }

    /// The result of an install is a set of `node_modules` trees. The CAS
    /// stores single files, so there is nothing here it could capture.
    fn cache_policy(&self, _inputs: &Block<PnpmInstallInputs>) -> CachePolicy {
        CachePolicy::Local
    }

    fn output_keys(&self, inputs: &Block<PnpmInstallInputs>) -> Vec<String> {
        vec![
            Path::new(workspace_dir(inputs))
                .join("node_modules/.modules.yaml")
                .to_string_lossy()
                .into_owned(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_resource() -> PnpmInstallResource {
        PnpmInstallResource {
            tracker: Arc::new(FileTracker::new()),
        }
    }

    #[test]
    fn resource_kind_is_build() {
        assert_eq!(Resource::kind(&test_resource()), ResourceKind::Build);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = Block::new(None, PnpmInstallInputs { frozen: true });
        let result = Resource::plan(&test_resource(), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "pnpm install --frozen-lockfile");
    }

    #[test]
    fn plan_update_when_frozen_changed() {
        let inputs = Block::new(None, PnpmInstallInputs { frozen: false });
        let prior = PnpmInstallState {
            frozen: true,
            node_modules_dirs: vec![],
        };
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn destroy_removes_all_node_modules_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::create_dir_all(root.join("pkg/node_modules")).unwrap();

        let state = PnpmInstallState {
            frozen: true,
            node_modules_dirs: vec![
                root.to_string_lossy().into_owned(),
                root.join("pkg").to_string_lossy().into_owned(),
            ],
        };
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        let state = Block::new(Some(root.to_string_lossy().into_owned()), state);
        Resource::destroy(&test_resource(), &state, &writer).unwrap();
        assert!(!root.join("node_modules").exists());
        assert!(!root.join("pkg/node_modules").exists());
    }
}
