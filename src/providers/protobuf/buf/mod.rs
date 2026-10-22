pub mod breaking;
pub mod format;
pub mod generate;
pub mod lint;
mod workspace;

use std::process::ExitStatus;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{Ambient, BoxError, DynResource, FuncSignature, Provider};
use crate::value::Value;

use workspace::Sources;

/// What a buf command operates on. Shared by all buf resources, which run
/// buf in the block's `dir`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, bit_derive::Schema)]
pub struct BufTarget {
    /// Buf input, relative to `dir` (defaults to the workspace in `dir`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
}

impl BufTarget {
    /// Run `buf <args...> [input]` in `dir`.
    fn run(&self, dir: Option<&str>, args: &[String], writer: &BlockWriter) -> Result<ExitStatus, BoxError> {
        super::run("buf", &self.args(args), dir, writer)
    }

    /// Plan description for `buf <args...> [input]`.
    fn describe(&self, args: &[String]) -> String {
        crate::providers::display_command("buf", &self.args(args))
    }

    fn args(&self, args: &[String]) -> Vec<String> {
        let mut args = args.to_vec();
        args.extend(self.input.clone());
        args
    }

    /// The `.proto` sources and buf configuration the command reads for its
    /// input, which defaults to the workspace in `dir`.
    fn sources(&self, dir: Option<&str>) -> Result<Sources, BoxError> {
        let mut sources = Sources::default();
        sources.add_input(dir, self.input.as_deref().unwrap_or("."))?;
        Ok(sources)
    }

    /// buf and the check plugins the input's `buf.yaml` names, which lint
    /// and breaking change checks run.
    fn check_ambient(&self, dir: Option<&str>) -> Vec<Ambient> {
        #[derive(Deserialize)]
        struct Config {
            #[serde(default)]
            plugins: Vec<CheckPlugin>,
        }
        #[derive(Deserialize)]
        struct CheckPlugin {
            plugin: PluginCommand,
        }

        let mut tools = vec![buf(dir)];
        // An input or configuration that cannot be read fails the buf
        // command itself.
        let Ok(sources) = self.sources(dir) else {
            return tools;
        };
        for path in sources.buf_yamls() {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            if let Ok(config) = serde_yml::from_str::<Config>(&text) {
                let programs = config.plugins.iter().filter_map(|p| p.plugin.program());
                tools.extend(programs.map(|program| super::plugin(&program, dir)));
            }
        }
        tools
    }
}

/// The buf CLI's version, as run in `dir`.
fn buf(dir: Option<&str>) -> Ambient {
    super::version("buf", dir)
}

/// A plugin command in buf configuration: a program, alone or followed by
/// its arguments.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PluginCommand {
    Program(String),
    WithArgs(Vec<String>),
}

impl PluginCommand {
    fn program(&self) -> Option<String> {
        match self {
            Self::Program(program) => Some(program.clone()),
            Self::WithArgs(command) => command.first().cloned(),
        }
    }
}

/// Buf provider with `generate`, `lint`, `format`, `format-check`, and
/// `breaking` resources.
pub struct BufProvider {
    tracker: Arc<FileTracker>,
}

impl BufProvider {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

impl Provider for BufProvider {
    fn name(&self) -> &str {
        "buf"
    }

    fn resources(&self) -> Vec<Box<dyn DynResource>> {
        vec![
            Box::new(generate::BufGenerateResource::new(self.tracker.clone())),
            Box::new(lint::BufLintResource::new(self.tracker.clone())),
            Box::new(format::BufFormatResource::new(self.tracker.clone())),
            Box::new(format::BufFormatCheckResource::new(self.tracker.clone())),
            Box::new(breaking::BufBreakingResource::new(self.tracker.clone())),
        ]
    }

    fn functions(&self) -> Vec<FuncSignature> {
        vec![]
    }

    fn call_function(&self, name: &str, _args: &[Value]) -> Result<Value, BoxError> {
        Err(format!("buf provider has no function '{name}'").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_registration() {
        let provider = BufProvider::new(Arc::new(FileTracker::default()));
        assert_eq!(provider.name(), "buf");
        let names: Vec<String> = provider.resources().iter().map(|r| r.name().to_owned()).collect();
        assert_eq!(names, ["generate", "lint", "format", "format-check", "breaking"]);
    }

    #[test]
    fn describe_appends_input() {
        let target = BufTarget {
            input: Some("proto".into()),
        };
        assert_eq!(target.describe(&["lint".into()]), "buf lint proto");
        assert_eq!(BufTarget::default().describe(&["lint".into()]), "buf lint");
    }

    #[test]
    fn sources_follow_the_input_to_its_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("proto/acme")).unwrap();
        std::fs::create_dir_all(root.join("other")).unwrap();
        std::fs::write(root.join("buf.yaml"), "version: v2\nmodules:\n  - path: proto\n").unwrap();
        std::fs::write(root.join("proto/acme/a.proto"), "").unwrap();
        std::fs::write(root.join("other/b.proto"), "").unwrap();
        let target = BufTarget {
            input: Some("proto/acme".into()),
        };
        let resolved = target
            .sources(Some(&root.to_string_lossy()))
            .unwrap()
            .hash(&FileTracker::default())
            .unwrap();
        let mut names: Vec<_> = resolved
            .keys()
            .map(|path| path.strip_prefix(&*root.to_string_lossy()).unwrap().to_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["/buf.yaml", "/proto/acme/a.proto"]);
    }

    #[test]
    fn check_tools_read_workspace_plugins_above_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("proto")).unwrap();
        std::fs::write(
            root.join("buf.yaml"),
            "version: v2\nmodules:\n  - path: proto\nplugins:\n  - plugin: buf-plugin-a\n",
        )
        .unwrap();
        let proto = root.join("proto").to_string_lossy().into_owned();
        assert_eq!(
            BufTarget::default().check_ambient(Some(&proto)),
            [
                crate::providers::version("buf", &["--version"], Some(&proto)),
                Ambient::Executable("buf-plugin-a".into()),
            ]
        );
    }

    #[test]
    fn check_tools_read_buf_yaml_plugins() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("buf.yaml"),
            "version: v2\nplugins:\n  - plugin: buf-plugin-a\n  - plugin: [bin/buf-plugin-b, --flag]\n",
        )
        .unwrap();
        let root = root.to_string_lossy().into_owned();
        assert_eq!(
            BufTarget::default().check_ambient(Some(&root)),
            [
                crate::providers::version("buf", &["--version"], Some(&root)),
                Ambient::Executable("buf-plugin-a".into()),
                Ambient::Executable(format!("{root}/bin/buf-plugin-b")),
            ]
        );
    }

    #[test]
    fn resolve_without_input_tracks_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("other")).unwrap();
        std::fs::write(root.join("other/b.proto"), "").unwrap();
        let resolved = BufTarget::default()
            .sources(Some(&root.to_string_lossy()))
            .unwrap()
            .hash(&FileTracker::default())
            .unwrap();
        assert!(
            resolved.keys().any(|path| path.ends_with("other/b.proto")),
            "{resolved:?}"
        );
    }
}
