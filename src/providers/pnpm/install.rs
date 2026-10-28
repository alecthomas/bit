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
    /// The name of each project's modules directory, the `modules-dir`
    /// setting. State from before it was recorded used the default.
    #[serde(default = "default_modules_dir")]
    pub modules_dir: String,
}

fn default_modules_dir() -> String {
    "node_modules".to_owned()
}

/// The files besides manifests and the lockfile that change what `pnpm
/// install` in `dir` installs, where they exist: the `.npmrc` files in
/// `dir` and the workspace root, the user and global config files `config`
/// names, the pnpmfile, and the patches `patchedDependencies` applies.
fn install_config_files(
    ws: &workspace::Workspace,
    dir: &Path,
    config: &serde_json::Value,
) -> Result<Vec<PathBuf>, BoxError> {
    let mut files = vec![dir.join(".npmrc"), ws.root.join(".npmrc")];
    for key in ["userconfig", "globalconfig", "npm-globalconfig"] {
        files.extend(config[key].as_str().map(PathBuf::from));
    }
    // The pnpmfile is relative to the lockfile's directory, the root.
    match config["pnpmfile"].as_str() {
        Some(pnpmfile) => files.push(ws.root.join(pnpmfile)),
        None => files.extend([".pnpmfile.cjs", ".pnpmfile.mjs"].map(|name| ws.root.join(name))),
    }
    // pnpm reports the patches `pnpm-workspace.yaml` lists as absolute paths,
    // but not those in the root manifest's `pnpm` field, which are relative
    // to the root.
    let patches = config["patched-dependencies"]
        .as_object()
        .into_iter()
        .flat_map(|p| p.values());
    files.extend(
        patches
            .filter_map(|patch| patch.as_str())
            .map(|patch| ws.root.join(patch)),
    );
    let manifest = std::fs::read_to_string(&ws.root_package_json)
        .map_err(|e| format!("pnpm: reading {}: {e}", ws.root_package_json.display()))?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest)
        .map_err(|e| format!("pnpm: parsing {}: {e}", ws.root_package_json.display()))?;
    let patches = manifest["pnpm"]["patchedDependencies"]
        .as_object()
        .into_iter()
        .flat_map(|p| p.values());
    files.extend(
        patches
            .filter_map(|patch| patch.as_str())
            .map(|patch| ws.root.join(patch)),
    );
    Ok(files)
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

    /// pnpm reads settings from `npm_config_*` and `pnpm_config_*`
    /// variables in either case, and `NODE_ENV=production` skips
    /// devDependencies.
    fn ambient(&self, inputs: &Block<PnpmInstallInputs>) -> Vec<Ambient> {
        let mut ambient = super::ambient(workspace_dir(inputs));
        ambient.push(Ambient::Variable("NODE_ENV".to_owned()));
        for prefix in ["npm_config_", "NPM_CONFIG_", "pnpm_config_", "PNPM_CONFIG_"] {
            ambient.push(Ambient::Variables(prefix.to_owned()));
        }
        ambient
    }

    fn resolve(&self, inputs: &Block<PnpmInstallInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let tracker = &*self.tracker;
        let dir = Path::new(workspace_dir(inputs));
        let ws = workspace::workspace(tracker, dir)?;
        let config = workspace::config(tracker, dir)?;
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
        files.extend(tracker.hash_files(&install_config_files(&ws, dir, &config)?)?);
        Ok(files)
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

        let ws = workspace::workspace(&self.tracker, Path::new(workspace_dir(inputs)))?;
        let mut node_modules_dirs: Vec<String> = vec![ws.root.to_string_lossy().into_owned()];
        for project in &ws.projects {
            if project.dir != ws.root {
                node_modules_dirs.push(project.dir.to_string_lossy().into_owned());
            }
        }
        let layout = self.layout(inputs);
        let node_modules_path = ws.root.join(&layout.modules_dir).to_string_lossy().into_owned();

        Ok(ApplyResult {
            outputs: PnpmInstallOutputs {
                path: node_modules_path,
            },
            state: Some(PnpmInstallState {
                frozen: inputs.frozen,
                node_modules_dirs,
                modules_dir: layout.modules_dir,
            }),
        })
    }

    fn destroy(&self, prior_state: &Block<PnpmInstallState>, writer: &BlockWriter) -> Result<(), BoxError> {
        for dir in &prior_state.node_modules_dirs {
            let nm: PathBuf = Path::new(dir).join(&prior_state.modules_dir);
            crate::providers::remove_path(&nm, writer)?;
        }
        Ok(())
    }

    /// The result of an install is a set of `node_modules` trees. The CAS
    /// stores single files, so there is nothing here it could capture.
    fn cache_policy(&self, _inputs: &Block<PnpmInstallInputs>) -> CachePolicy {
        CachePolicy::Local
    }

    /// Two files pnpm rewrites on every install, which stand in for the
    /// trees: hashing every package in them would cost more than a rerun.
    /// `.modules.yaml` records the layout and `lock.yaml` the packages the
    /// virtual store holds, so an install by hand, or a deleted or
    /// re-created tree, changes one of them.
    fn output_keys(&self, inputs: &Block<PnpmInstallInputs>) -> Vec<String> {
        let layout = self.layout(inputs);
        [
            layout.root.join(&layout.modules_dir).join(".modules.yaml"),
            layout.virtual_store_dir.join("lock.yaml"),
        ]
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
    }
}

/// Where `pnpm install` writes in a workspace.
struct Layout {
    root: PathBuf,
    /// The `modules-dir` setting: each project's modules directory, relative
    /// to the project.
    modules_dir: String,
    /// The `virtual-store-dir` setting, resolved against the root.
    virtual_store_dir: PathBuf,
}

impl PnpmInstallResource {
    /// The block's install layout, from the settings `pnpm config list`
    /// reports. Without them, pnpm's defaults in `dir`.
    fn layout(&self, inputs: &Block<PnpmInstallInputs>) -> Layout {
        let dir = Path::new(workspace_dir(inputs));
        let root = workspace::workspace(&self.tracker, dir).map_or_else(|_| dir.to_path_buf(), |ws| ws.root.clone());
        let config = workspace::config(&self.tracker, dir).unwrap_or_default();
        let modules_dir = config["modules-dir"].as_str().unwrap_or("node_modules").to_owned();
        let virtual_store_dir = match config["virtual-store-dir"].as_str() {
            Some(virtual_store_dir) => root.join(virtual_store_dir),
            None => root.join(&modules_dir).join(".pnpm"),
        };
        Layout {
            root,
            modules_dir,
            virtual_store_dir,
        }
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
    fn resolve_tracks_config_and_patches() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{ "name": "root", "pnpm": { "patchedDependencies": { "a@1.0.0": "patches/a.patch" } } }"#,
        )
        .unwrap();
        std::fs::write(
            root.join("pnpm-workspace.yaml"),
            "packages: []\npatchedDependencies:\n  b@1.0.0: patches/b.patch\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("patches")).unwrap();
        for file in [".npmrc", ".pnpmfile.cjs", "patches/a.patch", "patches/b.patch"] {
            std::fs::write(root.join(file), "").unwrap();
        }
        let inputs = Block::new(
            Some(root.to_string_lossy().into_owned()),
            PnpmInstallInputs { frozen: true },
        );

        let files = Resource::resolve(&test_resource(), &inputs).unwrap();
        for file in [".npmrc", ".pnpmfile.cjs", "patches/a.patch", "patches/b.patch"] {
            assert!(
                files.contains_key(&*root.join(file).to_string_lossy()),
                "{file}: {files:?}"
            );
        }

        let ambient = Resource::ambient(&test_resource(), &inputs);
        assert!(ambient.contains(&Ambient::Variable("NODE_ENV".into())));
        assert!(ambient.contains(&Ambient::Variables("npm_config_".into())));
    }

    #[test]
    fn plan_update_when_frozen_changed() {
        let inputs = Block::new(None, PnpmInstallInputs { frozen: false });
        let prior = PnpmInstallState {
            frozen: true,
            node_modules_dirs: vec![],
            modules_dir: default_modules_dir(),
        };
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn destroy_removes_all_node_modules_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("nm")).unwrap();
        std::fs::create_dir_all(root.join("pkg/nm")).unwrap();

        let state = PnpmInstallState {
            frozen: true,
            node_modules_dirs: vec![
                root.to_string_lossy().into_owned(),
                root.join("pkg").to_string_lossy().into_owned(),
            ],
            modules_dir: "nm".into(),
        };
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        let state = Block::new(Some(root.to_string_lossy().into_owned()), state);
        Resource::destroy(&test_resource(), &state, &writer).unwrap();
        assert!(!root.join("nm").exists());
        assert!(!root.join("pkg/nm").exists());
    }

    #[test]
    fn state_without_a_modules_dir_still_loads() {
        let state: PnpmInstallState = serde_json::from_str(r#"{"frozen": true, "node_modules_dirs": []}"#).unwrap();
        assert_eq!(state.modules_dir, "node_modules");
    }

    #[test]
    fn output_keys_follow_the_configured_layout_at_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("package.json"), r#"{ "name": "root" }"#).unwrap();
        std::fs::write(root.join("pnpm-workspace.yaml"), "packages:\n  - app\n").unwrap();
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::write(root.join("app/package.json"), r#"{ "name": "app" }"#).unwrap();
        let keys = |dir: &Path| {
            let inputs = Block::new(
                Some(dir.to_string_lossy().into_owned()),
                PnpmInstallInputs { frozen: true },
            );
            Resource::output_keys(&test_resource(), &inputs)
        };
        let path = |rel: &str| root.join(rel).to_string_lossy().into_owned();

        assert_eq!(
            keys(&root.join("app")),
            [path("node_modules/.modules.yaml"), path("node_modules/.pnpm/lock.yaml")]
        );
        std::fs::write(root.join(".npmrc"), "modules-dir=nm\nvirtual-store-dir=.store\n").unwrap();
        assert_eq!(keys(&root), [path("nm/.modules.yaml"), path(".store/lock.yaml")]);
    }
}
