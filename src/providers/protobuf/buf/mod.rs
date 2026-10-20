pub mod breaking;
pub mod format;
pub mod generate;
pub mod lint;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{Ambient, BoxError, DynResource, FuncSignature, Provider};
use crate::sha256::SHA256;
use crate::value::Value;

/// Configuration files that change what every buf command does.
const CONFIG_FILES: [&str; 3] = ["buf.yaml", "buf.lock", "buf.work.yaml"];

/// What a buf command operates on and where it runs. Shared by all buf
/// resources.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, bit_derive::Schema)]
pub struct BufTarget {
    /// Buf input, relative to `dir` (defaults to the workspace in `dir`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// Working directory for the command
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

impl BufTarget {
    /// Run `buf <args...> [input]`.
    fn run(&self, args: &[String], writer: &BlockWriter) -> Result<ExitStatus, BoxError> {
        super::run("buf", &self.args(args), self.dir.as_deref(), writer)
    }

    /// Plan description for `buf <args...> [input]`.
    fn describe(&self, args: &[String]) -> String {
        format!("buf {}", self.args(args).join(" "))
    }

    fn args(&self, args: &[String]) -> Vec<String> {
        let mut args = args.to_vec();
        args.extend(self.input.clone());
        args
    }

    /// Hash the `.proto` sources and buf configuration the command reads,
    /// plus `extra` files relative to `dir`.
    ///
    /// An input that is not a local directory, such as a Git or BSR
    /// reference, contributes only the configuration in `dir`.
    fn resolve(&self, extra: &[&str], tracker: &FileTracker) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let dir = self.dir.as_deref();
        let config_dirs = self.config_dirs();
        let source_root = config_dirs.last().expect("dir is always a config dir");
        let mut hashes = super::hash_protos(std::slice::from_ref(source_root), tracker)?;
        let files: Vec<PathBuf> = config_dirs
            .iter()
            .flat_map(|config_dir| CONFIG_FILES.iter().map(|name| config_dir.join(name)))
            .chain(extra.iter().map(|path| super::in_dir(dir, path)))
            .filter(|path| path.is_file())
            .collect();
        hashes.extend(tracker.hash_files(&files)?);
        Ok(hashes)
    }

    /// The buf CLI's version, as run in `dir`.
    fn buf(&self) -> Ambient {
        super::version("buf", self.dir.as_deref())
    }

    /// `dir`, then the input when it is a local directory, which buf also
    /// reads as a module root.
    fn config_dirs(&self) -> Vec<PathBuf> {
        let dir = self.dir.as_deref();
        let mut config_dirs = vec![super::in_dir(dir, "")];
        if let Some(input) = &self.input {
            let input = super::in_dir(dir, input);
            if input.is_dir() {
                config_dirs.push(input);
            }
        }
        config_dirs
    }

    /// buf and the check plugins `buf.yaml` names, which lint and breaking
    /// change checks run.
    fn check_ambient(&self) -> Vec<Ambient> {
        #[derive(Deserialize)]
        struct Config {
            #[serde(default)]
            plugins: Vec<CheckPlugin>,
        }
        #[derive(Deserialize)]
        struct CheckPlugin {
            plugin: PluginCommand,
        }

        let dir = self.dir.as_deref();
        let mut tools = vec![self.buf()];
        for config_dir in self.config_dirs() {
            let Ok(text) = std::fs::read_to_string(config_dir.join("buf.yaml")) else {
                continue;
            };
            // An unreadable configuration fails the buf command itself.
            if let Ok(config) = serde_yml::from_str::<Config>(&text) {
                let programs = config.plugins.iter().filter_map(|p| p.plugin.program());
                tools.extend(programs.map(|program| super::plugin(&program, dir)));
            }
        }
        tools
    }
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
            dir: None,
        };
        assert_eq!(target.describe(&["lint".into()]), "buf lint proto");
        assert_eq!(BufTarget::default().describe(&["lint".into()]), "buf lint");
    }

    #[test]
    fn resolve_tracks_input_sources_and_config() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("proto/acme")).unwrap();
        std::fs::create_dir_all(root.join("other")).unwrap();
        std::fs::write(root.join("buf.yaml"), "version: v2\n").unwrap();
        std::fs::write(root.join("buf.gen.yaml"), "version: v2\n").unwrap();
        std::fs::write(root.join("proto/acme/a.proto"), "").unwrap();
        std::fs::write(root.join("other/b.proto"), "").unwrap();
        let target = BufTarget {
            input: Some("proto".into()),
            dir: Some(root.to_string_lossy().into_owned()),
        };
        let resolved = target.resolve(&["buf.gen.yaml"], &FileTracker::default()).unwrap();
        let mut names: Vec<_> = resolved
            .keys()
            .map(|path| path.strip_prefix(&*root.to_string_lossy()).unwrap().to_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["/buf.gen.yaml", "/buf.yaml", "/proto/acme/a.proto"]);
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
        let target = BufTarget {
            input: None,
            dir: Some(root.clone()),
        };
        assert_eq!(
            target.check_ambient(),
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
        let target = BufTarget {
            input: None,
            dir: Some(root.to_string_lossy().into_owned()),
        };
        let resolved = target.resolve(&[], &FileTracker::default()).unwrap();
        assert!(
            resolved.keys().any(|path| path.ends_with("other/b.proto")),
            "{resolved:?}"
        );
    }
}
