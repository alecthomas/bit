use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{Ambient, ApplyResult, BoxError, CachePolicy, Changes, PlanResult, Resource, ResourceKind};
use crate::sha256::SHA256;

use super::{BufTarget, PluginCommand};

const DEFAULT_TEMPLATE: &str = "buf.gen.yaml";

/// Generate code from Protobuf files with buf generate
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bit_derive::Schema)]
pub struct BufGenerateInputs {
    #[serde(flatten)]
    pub target: BufTarget,
    /// Generation template path, relative to `dir` (defaults to "buf.gen.yaml")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// Extra flags passed to buf generate
    #[serde(default)]
    pub flags: Vec<String>,
}

impl BufGenerateInputs {
    fn template(&self) -> &str {
        self.template.as_deref().unwrap_or(DEFAULT_TEMPLATE)
    }

    fn args(&self) -> Vec<String> {
        let mut args = vec!["generate".to_owned()];
        if let Some(template) = &self.template {
            args.extend(["--template".to_owned(), template.clone()]);
        }
        args.extend(self.flags.iter().cloned());
        args
    }

    fn read_template(&self) -> Result<Template, BoxError> {
        let path = super::super::in_dir(self.target.dir.as_deref(), self.template());
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("failed to read generation template {}: {e}", path.display()))?;
        Ok(serde_yml::from_str(&text)
            .map_err(|e| format!("failed to parse generation template {}: {e}", path.display()))?)
    }

    /// Output paths of the template's plugins, as bit's process sees them.
    fn outputs(&self) -> Result<Vec<PathBuf>, BoxError> {
        let dir = self.target.dir.as_deref();
        Ok(self
            .read_template()?
            .plugins
            .iter()
            .map(|plugin| super::super::in_dir(dir, &plugin.out))
            .collect())
    }

    /// buf and the local executables the template's plugins run. A
    /// template that cannot be read fails `buf generate` itself.
    fn ambient(&self) -> Vec<Ambient> {
        let dir = self.target.dir.as_deref();
        let mut tools = vec![super::super::version("buf", dir)];
        if let Ok(template) = self.read_template() {
            tools.extend(template.plugins.iter().flat_map(|plugin| plugin.ambient(dir)));
        }
        tools
    }
}

/// The subset of `buf.gen.yaml` (v1 or v2) that names plugins and their
/// outputs.
#[derive(Deserialize)]
struct Template {
    #[serde(default)]
    plugins: Vec<TemplatePlugin>,
}

#[derive(Deserialize)]
struct TemplatePlugin {
    out: String,
    /// v2: a local plugin executable.
    #[serde(default)]
    local: Option<PluginCommand>,
    /// v2: a generator built into protoc.
    #[serde(default)]
    protoc_builtin: Option<String>,
    /// v1 and v2: the protoc that runs built-in generators.
    #[serde(default)]
    protoc_path: Option<PluginCommand>,
    /// v1: a plugin name, or a remote plugin when it contains `/`.
    #[serde(default, alias = "name")]
    plugin: Option<String>,
    /// v1: the plugin executable.
    #[serde(default)]
    path: Option<PluginCommand>,
}

impl TemplatePlugin {
    /// Local executables this plugin runs from `dir`. Remote plugins run on
    /// the Buf Schema Registry; their version is part of the template.
    fn ambient(&self, dir: Option<&str>) -> Vec<Ambient> {
        let protoc = || {
            let program = self.protoc_path.as_ref().and_then(PluginCommand::program);
            super::super::version(program.as_deref().unwrap_or("protoc"), dir)
        };
        let plugin = |program: &str| super::super::plugin(program, dir);
        if let Some(local) = &self.local {
            return local.program().iter().map(|program| plugin(program)).collect();
        }
        if self.protoc_builtin.is_some() {
            return vec![protoc()];
        }
        if let Some(path) = &self.path {
            return path.program().iter().map(|program| plugin(program)).collect();
        }
        match &self.plugin {
            Some(name) if name.contains('/') => vec![],
            // buf v1 prefers a `protoc-gen-*` plugin and falls back to the
            // generator built into protoc.
            Some(name) if super::super::PROTOC_BUILTIN_GENERATORS.contains(&name.as_str()) => {
                vec![plugin(&format!("protoc-gen-{name}")), protoc()]
            }
            Some(name) => vec![plugin(&format!("protoc-gen-{name}"))],
            None => vec![],
        }
    }
}

/// Outputs from a `buf.generate` block.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct BufGenerateOutputs {
    /// Output paths named by the generation template's plugins
    pub paths: Vec<String>,
}

/// Persisted state for a `buf.generate` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufGenerateState {
    #[serde(flatten)]
    pub inputs: BufGenerateInputs,
    /// Generated outputs removed on destroy.
    #[serde(default)]
    pub outputs: Vec<String>,
}

pub struct BufGenerateResource {
    tracker: Arc<FileTracker>,
}

impl BufGenerateResource {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

impl Resource for BufGenerateResource {
    type State = BufGenerateState;
    type Inputs = BufGenerateInputs;
    type Outputs = BufGenerateOutputs;

    fn name(&self) -> &str {
        "generate"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn resolve(&self, inputs: &BufGenerateInputs) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let tracker = &*self.tracker;
        inputs.target.resolve(&[inputs.template()], tracker)
    }

    fn ambient(&self, inputs: &BufGenerateInputs) -> Vec<Ambient> {
        inputs.ambient()
    }

    fn plan(&self, inputs: &BufGenerateInputs, prior_state: Option<&BufGenerateState>) -> Result<PlanResult, BoxError> {
        Ok(super::super::plan(
            inputs.target.describe(&inputs.args()),
            prior_state.map(|prior| &prior.inputs),
            inputs,
        ))
    }

    fn apply(
        &self,
        inputs: &BufGenerateInputs,
        _prior_state: Option<&BufGenerateState>,
        _changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<BufGenerateState, BufGenerateOutputs>, BoxError> {
        let outputs = inputs.outputs()?;
        let status = inputs.target.run(&inputs.args(), writer)?;
        if !status.success() {
            return Err(format!("`buf generate` exited with {status}").into());
        }
        Ok(ApplyResult {
            outputs: BufGenerateOutputs {
                paths: outputs.iter().map(|path| path.to_string_lossy().into_owned()).collect(),
            },
            state: Some(BufGenerateState {
                inputs: inputs.clone(),
                outputs: super::super::removable_outputs(&outputs)?,
            }),
        })
    }

    fn destroy(&self, prior_state: &BufGenerateState, writer: &BlockWriter) -> Result<(), BoxError> {
        for output in &prior_state.outputs {
            crate::providers::remove_path(Path::new(output), writer)?;
        }
        Ok(())
    }

    /// Plugins are either remote, run on the Buf Schema Registry, or local
    /// executables on `PATH`. Neither is part of the action key, so the key
    /// cannot describe what produced the generated files.
    fn cache_policy(&self, _inputs: &BufGenerateInputs) -> CachePolicy {
        CachePolicy::Local
    }

    fn output_keys(&self, inputs: &BufGenerateInputs) -> Vec<String> {
        inputs
            .outputs()
            .and_then(|outputs| super::super::removable_outputs(&outputs))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::PlanAction;

    fn test_resource() -> BufGenerateResource {
        BufGenerateResource::new(Arc::new(FileTracker::default()))
    }

    /// Inputs reading `template` from a fresh directory.
    fn with_template(template: &str) -> (tempfile::TempDir, BufGenerateInputs) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(DEFAULT_TEMPLATE), template).unwrap();
        let inputs = BufGenerateInputs {
            target: BufTarget {
                input: None,
                dir: Some(dir.path().to_string_lossy().into_owned()),
            },
            template: None,
            flags: vec![],
        };
        (dir, inputs)
    }

    #[test]
    fn reads_v1_template_outputs_and_programs() {
        let (dir, inputs) = with_template(
            "version: v1\nplugins:\n  - plugin: go\n    out: gen/go\n  - name: java\n    out: gen/java\n  - plugin: buf.build/grpc/go\n    out: gen/grpc\n  - plugin: ts\n    path: [bin/protoc-gen-ts, --flag]\n    out: gen/ts\n",
        );
        let outputs: Vec<_> = ["gen/go", "gen/java", "gen/grpc", "gen/ts"]
            .iter()
            .map(|out| dir.path().join(out))
            .collect();
        assert_eq!(inputs.outputs().unwrap(), outputs);
        let dir = inputs.target.dir.as_deref();
        assert_eq!(
            inputs.ambient(),
            [
                crate::providers::version("buf", &["--version"], dir),
                Ambient::Executable("protoc-gen-go".into()),
                Ambient::Executable("protoc-gen-java".into()),
                crate::providers::version("protoc", &["--version"], dir),
                Ambient::Executable(format!("{}/bin/protoc-gen-ts", dir.unwrap())),
            ]
        );
    }

    #[test]
    fn reads_v2_template_outputs_and_programs() {
        let (dir, inputs) = with_template(
            "version: v2\nplugins:\n  - remote: buf.build/protocolbuffers/go\n    out: gen/go\n  - local: protoc-gen-es\n    out: gen/ts\n  - protoc_builtin: java\n    protoc_path: [bin/protoc, --x]\n    out: gen/java\n",
        );
        let outputs: Vec<_> = ["gen/go", "gen/ts", "gen/java"]
            .iter()
            .map(|out| dir.path().join(out))
            .collect();
        assert_eq!(inputs.outputs().unwrap(), outputs);
        let dir = inputs.target.dir.as_deref();
        assert_eq!(
            inputs.ambient(),
            [
                crate::providers::version("buf", &["--version"], dir),
                Ambient::Executable("protoc-gen-es".into()),
                crate::providers::version("bin/protoc", &["--version"], dir),
            ]
        );
    }

    #[test]
    fn plan_describes_template_and_input() {
        let inputs: BufGenerateInputs =
            serde_json::from_str(r#"{"input": "proto", "template": "buf.gen.go.yaml"}"#).unwrap();
        let result = Resource::plan(&test_resource(), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "buf generate --template buf.gen.go.yaml proto");
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs: BufGenerateInputs = serde_json::from_str("{}").unwrap();
        let prior = BufGenerateState {
            inputs: inputs.clone(),
            outputs: vec!["gen".into()],
        };
        let result = Resource::plan(&test_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn output_keys_skip_outputs_inside_the_source_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("proto")).unwrap();
        std::fs::write(root.join("proto/a.proto"), "").unwrap();
        std::fs::write(
            root.join(DEFAULT_TEMPLATE),
            "version: v2\nplugins:\n  - local: protoc-gen-go\n    out: gen/go\n  - local: protoc-gen-go\n    out: proto\n",
        )
        .unwrap();
        let inputs = BufGenerateInputs {
            target: BufTarget {
                input: None,
                dir: Some(root.to_string_lossy().into_owned()),
            },
            template: None,
            flags: vec![],
        };
        assert_eq!(
            Resource::output_keys(&test_resource(), &inputs),
            [root.join("gen/go").to_string_lossy().into_owned()]
        );
    }

    #[test]
    fn resolve_tracks_template() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("gen.yaml"), "version: v2\n").unwrap();
        let inputs = BufGenerateInputs {
            target: BufTarget {
                input: None,
                dir: Some(root.to_string_lossy().into_owned()),
            },
            template: Some("gen.yaml".into()),
            flags: vec![],
        };
        let resolved = Resource::resolve(&test_resource(), &inputs).unwrap();
        assert!(resolved.keys().any(|path| path.ends_with("gen.yaml")), "{resolved:?}");
    }

    #[test]
    fn destroy_removes_recorded_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let output_dir = dir.path().join("gen");
        std::fs::create_dir(&output_dir).unwrap();
        std::fs::write(output_dir.join("a.pb.go"), "package a").unwrap();
        let state = BufGenerateState {
            inputs: serde_json::from_str("{}").unwrap(),
            outputs: vec![output_dir.to_string_lossy().into_owned()],
        };
        let output = crate::output::Output::new(&[]);
        Resource::destroy(&test_resource(), &state, &output.writer("generate")).unwrap();
        assert!(!output_dir.exists());
    }
}
