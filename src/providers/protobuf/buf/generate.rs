use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{
    Ambient, ApplyResult, Block, BoxError, CachePolicy, Changes, PlanResult, Resource, ResourceKind,
};
use crate::sha256::SHA256;

use super::{BufTarget, PluginCommand, Sources};

const DEFAULT_TEMPLATE: &str = "buf.gen.yaml";

/// Generate code from Protobuf files with buf generate. The sources, outputs, and
/// plugins are discovered from the buf workspace and generation template.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bit_derive::Schema)]
pub struct BufGenerateInputs {
    #[serde(flatten)]
    pub target: BufTarget,
    /// Generation template path relative to `dir`, or inline YAML or JSON (defaults to "buf.gen.yaml")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// Extra flags passed to buf generate. A config file `--config` names is tracked as an input.
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
}

impl Block<BufGenerateInputs> {
    /// The template file, when `template` names one rather than holding
    /// the template itself. buf decides by extension.
    fn template_file(&self) -> Option<PathBuf> {
        let template = self.template();
        let extension = Path::new(template).extension().and_then(|ext| ext.to_str());
        matches!(extension, Some("yaml" | "yml" | "json")).then(|| PathBuf::from(self.path(template)))
    }

    fn read_template(&self) -> Result<Template, BoxError> {
        let Some(path) = self.template_file() else {
            return Ok(serde_yml::from_str(self.template())
                .map_err(|e| format!("failed to parse inline generation template: {e}"))?);
        };
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("failed to read generation template {}: {e}", path.display()))?;
        Ok(serde_yml::from_str(&text)
            .map_err(|e| format!("failed to parse generation template {}: {e}", path.display()))?)
    }

    /// What buf reads: the input, or else the template's inputs, or else
    /// the workspace in `dir`; plus the template itself.
    fn sources(&self) -> Result<Sources, BoxError> {
        let template_inputs = match (&self.target.input, self.read_template()) {
            (None, Ok(template)) if !template.inputs.is_empty() => Some(template.inputs),
            _ => None,
        };
        let mut sources = match template_inputs {
            Some(inputs) => {
                let mut sources = Sources::default();
                for path in inputs.iter().filter_map(TemplateInput::path) {
                    sources.add_input(self.dir.as_deref(), path)?;
                }
                if let Some(config) = super::config_file(self.dir.as_deref(), &self.flags) {
                    sources.add_config(config);
                }
                sources
            }
            None => self.target.sources(self.dir.as_deref(), &self.flags)?,
        };
        if let Some(path) = self.template_file() {
            sources.add_file(path);
        }
        Ok(sources)
    }

    /// Output paths of the template's plugins, as bit's process sees them.
    /// buf prefixes each with `--output`, even an absolute one.
    fn outputs(&self) -> Result<Vec<PathBuf>, BoxError> {
        let dir = self.dir.as_deref();
        let base = super::flag_value(&self.flags, "output", Some("o")).filter(|base| !base.is_empty() && *base != ".");
        Ok(self
            .read_template()?
            .plugins
            .iter()
            .map(|plugin| match base {
                Some(base) => Path::new(base).join(plugin.out.trim_start_matches('/')),
                None => PathBuf::from(&plugin.out),
            })
            .map(|out| Path::new(dir.unwrap_or("")).join(out))
            .collect())
    }

    /// buf and the local executables the template's plugins run. A
    /// template that cannot be read fails `buf generate` itself.
    fn ambient(&self) -> Vec<Ambient> {
        let dir = self.dir.as_deref();
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
    /// v2: what to generate from when the command names no input.
    #[serde(default)]
    inputs: Vec<TemplateInput>,
}

/// A v2 template input. Only the kinds that can name a local path are
/// listed; a `module` or `git_repo` input has no local sources.
#[derive(Deserialize)]
struct TemplateInput {
    #[serde(default)]
    directory: Option<String>,
    #[serde(default)]
    proto_file: Option<String>,
    #[serde(default)]
    tarball: Option<String>,
    #[serde(default)]
    zip_archive: Option<String>,
    #[serde(default)]
    binary_image: Option<String>,
    #[serde(default)]
    json_image: Option<String>,
    #[serde(default)]
    txt_image: Option<String>,
    #[serde(default)]
    yaml_image: Option<String>,
}

impl TemplateInput {
    fn path(&self) -> Option<&str> {
        [
            &self.directory,
            &self.proto_file,
            &self.tarball,
            &self.zip_archive,
            &self.binary_image,
            &self.json_image,
            &self.txt_image,
            &self.yaml_image,
        ]
        .into_iter()
        .find_map(Option::as_deref)
    }
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

    fn resolve(&self, inputs: &Block<BufGenerateInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        inputs.sources()?.hash(&self.tracker)
    }

    fn ambient(&self, inputs: &Block<BufGenerateInputs>) -> Vec<Ambient> {
        inputs.ambient()
    }

    fn plan(
        &self,
        inputs: &Block<BufGenerateInputs>,
        prior_state: Option<&BufGenerateState>,
    ) -> Result<PlanResult, BoxError> {
        Ok(super::super::plan(
            inputs.target.describe(&inputs.args()),
            prior_state.map(|prior| &prior.inputs),
            inputs,
        ))
    }

    fn apply(
        &self,
        inputs: &Block<BufGenerateInputs>,
        _prior_state: Option<&BufGenerateState>,
        _changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<BufGenerateState, BufGenerateOutputs>, BoxError> {
        let outputs = inputs.outputs()?;
        let status = inputs.target.run(inputs.dir.as_deref(), &inputs.args(), writer)?;
        if !status.success() {
            return Err(format!("`buf generate` exited with {status}").into());
        }
        Ok(ApplyResult {
            outputs: BufGenerateOutputs {
                paths: outputs.iter().map(|path| path.to_string_lossy().into_owned()).collect(),
            },
            state: Some(BufGenerateState {
                inputs: inputs.value.clone(),
                outputs: super::super::removable_outputs(&outputs)?,
            }),
        })
    }

    fn destroy(&self, prior_state: &Block<BufGenerateState>, writer: &BlockWriter) -> Result<(), BoxError> {
        for output in &prior_state.outputs {
            crate::providers::remove_path(Path::new(output), writer)?;
        }
        Ok(())
    }

    /// Plugins are either remote, run on the Buf Schema Registry, or local
    /// executables on `PATH`. Neither is part of the action key, so the key
    /// cannot describe what produced the generated files.
    fn cache_policy(&self, _inputs: &Block<BufGenerateInputs>) -> CachePolicy {
        CachePolicy::Local
    }

    fn output_keys(&self, inputs: &Block<BufGenerateInputs>) -> Vec<String> {
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

    /// Inputs run in `dir`.
    fn in_dir(dir: &Path, template: Option<&str>) -> Block<BufGenerateInputs> {
        Block::new(
            Some(dir.to_string_lossy().into_owned()),
            BufGenerateInputs {
                target: BufTarget::default(),
                template: template.map(str::to_owned),
                flags: vec![],
            },
        )
    }

    /// Inputs reading `template` from a fresh directory.
    fn with_template(template: &str) -> (tempfile::TempDir, Block<BufGenerateInputs>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(DEFAULT_TEMPLATE), template).unwrap();
        let inputs = in_dir(dir.path(), None);
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
        let dir = inputs.dir.as_deref();
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
        let dir = inputs.dir.as_deref();
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
        let inputs = Block::new(None, inputs);
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
        let inputs = Block::new(None, inputs);
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
        let inputs = in_dir(root, None);
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
        let inputs = in_dir(root, Some("gen.yaml"));
        let resolved = Resource::resolve(&test_resource(), &inputs).unwrap();
        assert!(resolved.keys().any(|path| path.ends_with("gen.yaml")), "{resolved:?}");
    }

    #[test]
    fn resolve_follows_template_inputs_without_an_input() {
        let (dir, mut inputs) =
            with_template("version: v2\ninputs:\n  - directory: proto\n  - module: buf.build/acme/api\n");
        let root = dir.path();
        std::fs::create_dir_all(root.join("proto")).unwrap();
        std::fs::create_dir_all(root.join("other")).unwrap();
        std::fs::write(root.join("proto/a.proto"), "").unwrap();
        std::fs::write(root.join("other/b.proto"), "").unwrap();
        let names = |inputs: &Block<BufGenerateInputs>| -> Vec<String> {
            let mut names: Vec<_> = Resource::resolve(&test_resource(), inputs)
                .unwrap()
                .into_keys()
                .map(|path| Path::new(&path).file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        assert_eq!(names(&inputs), ["a.proto", "buf.gen.yaml"]);
        // An input on the command line replaces the template's.
        inputs.target.input = Some("other".into());
        assert_eq!(names(&inputs), ["b.proto", "buf.gen.yaml"]);
    }

    #[test]
    fn reads_inline_template() {
        let inputs = Block::new(
            None,
            BufGenerateInputs {
                target: BufTarget::default(),
                template: Some(r#"{"version": "v2", "plugins": [{"local": "protoc-gen-go", "out": "gen"}]}"#.into()),
                flags: vec![],
            },
        );
        assert_eq!(inputs.outputs().unwrap(), [PathBuf::from("gen")]);
        assert!(inputs.template_file().is_none());
    }

    #[test]
    fn output_flag_prefixes_plugin_outputs() {
        let (dir, mut inputs) = with_template(
            "version: v2\nplugins:\n  - local: protoc-gen-go\n    out: gen\n  - local: protoc-gen-es\n    out: /abs\n",
        );
        for flags in [
            vec!["-o", "base"],
            vec!["--output=base"],
            vec!["--output", "x", "-obase"],
        ] {
            inputs.flags = flags.into_iter().map(str::to_owned).collect();
            assert_eq!(
                inputs.outputs().unwrap(),
                [dir.path().join("base/gen"), dir.path().join("base/abs")]
            );
        }
        inputs.flags = vec!["--output=.".into()];
        assert_eq!(
            inputs.outputs().unwrap(),
            [dir.path().join("gen"), PathBuf::from("/abs")]
        );
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
        Resource::destroy(&test_resource(), &Block::new(None, state), &output.writer("generate")).unwrap();
        assert!(!output_dir.exists());
    }
}
