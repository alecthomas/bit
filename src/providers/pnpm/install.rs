use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::output::{BlockWriter, Event};
use crate::provider::{ApplyResult, BoxError, PlanAction, PlanResult, ResolvedFile, Resource, ResourceKind};

use super::{run_pnpm, workspace};

fn default_dir() -> String {
    ".".to_owned()
}

fn default_frozen() -> bool {
    true
}

/// Install pnpm workspace dependencies.
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct PnpmInstallInputs {
    /// Workspace root directory (defaults to the current directory)
    #[serde(default = "default_dir")]
    pub dir: String,
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
    pub dir: String,
    pub frozen: bool,
    /// Absolute paths of every directory that contains a `node_modules/`.
    /// Used by `destroy` to remove them all.
    pub node_modules_dirs: Vec<String>,
}

pub struct PnpmInstallResource;

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

    fn resolve(&self, inputs: &PnpmInstallInputs) -> Result<Vec<ResolvedFile>, BoxError> {
        workspace::with_workspace(Path::new(&inputs.dir), |ws| {
            let mut files = vec![ResolvedFile::Input(ws.root_package_json.clone())];
            if let Some(lf) = &ws.lockfile {
                files.push(ResolvedFile::Input(lf.clone()));
            }
            if let Some(y) = &ws.workspace_yaml {
                files.push(ResolvedFile::Input(y.clone()));
            }
            for pkg_dir in ws.packages.values() {
                let pj = pkg_dir.join("package.json");
                if pj.exists() {
                    files.push(ResolvedFile::Input(pj));
                }
            }
            files.push(ResolvedFile::Output(ws.root.join("node_modules").join(".modules.yaml")));
            Ok(files)
        })
    }

    fn plan(&self, inputs: &PnpmInstallInputs, prior_state: Option<&PnpmInstallState>) -> Result<PlanResult, BoxError> {
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
        let action = if prior.dir != inputs.dir || prior.frozen != inputs.frozen {
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
        inputs: &PnpmInstallInputs,
        _prior_state: Option<&PnpmInstallState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<PnpmInstallState, PnpmInstallOutputs>, BoxError> {
        let mut args = vec!["install".to_owned()];
        if inputs.frozen {
            args.push("--frozen-lockfile".to_owned());
        }
        run_pnpm(&args, Some(&inputs.dir), writer)?;

        let (node_modules_path, node_modules_dirs) = workspace::with_workspace(Path::new(&inputs.dir), |ws| {
            let mut dirs: Vec<String> = vec![ws.root.to_string_lossy().into_owned()];
            for pkg_dir in ws.packages.values() {
                if pkg_dir != &ws.root {
                    dirs.push(pkg_dir.to_string_lossy().into_owned());
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
                dir: inputs.dir.clone(),
                frozen: inputs.frozen,
                node_modules_dirs,
            }),
        })
    }

    fn destroy(&self, prior_state: &PnpmInstallState, writer: &BlockWriter) -> Result<(), BoxError> {
        for dir in &prior_state.node_modules_dirs {
            let nm: PathBuf = Path::new(dir).join("node_modules");
            if nm.is_dir() {
                writer.event(Event::Starting, &format!("rm -rf {}", nm.display()));
                fs::remove_dir_all(&nm).ok();
            }
        }
        Ok(())
    }

    fn refresh(
        &self,
        prior_state: &PnpmInstallState,
    ) -> Result<ApplyResult<PnpmInstallState, PnpmInstallOutputs>, BoxError> {
        let path = Path::new(&prior_state.dir)
            .join("node_modules")
            .to_string_lossy()
            .into_owned();
        Ok(ApplyResult {
            outputs: PnpmInstallOutputs { path },
            state: Some(prior_state.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_kind_is_build() {
        assert_eq!(Resource::kind(&PnpmInstallResource), ResourceKind::Build);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = PnpmInstallInputs {
            dir: ".".into(),
            frozen: true,
        };
        let result = Resource::plan(&PnpmInstallResource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "pnpm install --frozen-lockfile");
    }

    #[test]
    fn plan_update_when_frozen_changed() {
        let inputs = PnpmInstallInputs {
            dir: ".".into(),
            frozen: false,
        };
        let prior = PnpmInstallState {
            dir: ".".into(),
            frozen: true,
            node_modules_dirs: vec![],
        };
        let result = Resource::plan(&PnpmInstallResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn destroy_removes_all_node_modules_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("node_modules")).unwrap();
        fs::create_dir_all(root.join("pkg/node_modules")).unwrap();

        let state = PnpmInstallState {
            dir: root.to_string_lossy().into_owned(),
            frozen: true,
            node_modules_dirs: vec![
                root.to_string_lossy().into_owned(),
                root.join("pkg").to_string_lossy().into_owned(),
            ],
        };
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        Resource::destroy(&PnpmInstallResource, &state, &writer).unwrap();
        assert!(!root.join("node_modules").exists());
        assert!(!root.join("pkg/node_modules").exists());
    }
}
