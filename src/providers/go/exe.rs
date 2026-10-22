use std::collections::BTreeMap;
use std::fs;
use std::io::BufReader;
use std::path::{Component, Path};
use std::process::{Command, Stdio};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::cache::{ArtifactRef, Cas};
use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{
    Ambient, ApplyResult, Block, BoxError, CachePolicy, Changes, MaterializeResult, OutputFile, PlanAction, PlanResult,
    Resource, ResourceKind, restore_output_files,
};
use crate::sha256::SHA256;

use super::GoEnv;
use super::scanner::{self, Tests};

/// Build a Go binary
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct GoExeInputs {
    /// Go package to build (e.g. "./cmd/myapp")
    pub package: String,
    /// Output binary path (defaults to the name `go build` gives it: the last element of the package's import path)
    #[serde(default)]
    pub output: Option<String>,
    /// Extra flags passed to go build
    #[serde(default)]
    pub flags: Vec<String>,
    /// Input file glob patterns (in addition to Go sources)
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(flatten)]
    pub env: GoEnv,
}

/// Outputs from a `go.exe` block.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct GoExeOutputs {
    /// Path to the built binary
    pub path: String,
}

/// Persisted state for a `go.exe` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoExeState {
    pub package: String,
    pub output: String,
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub env: GoEnv,
}

pub struct GoExeResource {
    tracker: Arc<FileTracker>,
}

impl GoExeResource {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }

    /// `output`, or the name `go build` gives the binary: the last element
    /// of the package's import path. A relative pattern such as `.` names a
    /// directory, so taking its text instead would make the output the
    /// directory itself.
    fn output_path(inputs: &Block<GoExeInputs>) -> String {
        inputs.output.clone().unwrap_or_else(|| {
            let package = &inputs.package;
            let import_path = if package.starts_with('.') {
                let dir = inputs.dir.as_deref().map(Path::new);
                // Without a module `go build` fails, so only the output key
                // matters, and it must not name a directory such as `.`.
                scanner::package_import_path(dir, package).unwrap_or_else(|_| {
                    let pkg_dir = Path::new(inputs.dir.as_deref().unwrap_or(".")).join(package);
                    let pkg_dir = pkg_dir.canonicalize().unwrap_or(pkg_dir);
                    pkg_dir
                        .components()
                        .rev()
                        .find_map(|c| match c {
                            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
                            _ => None,
                        })
                        .unwrap_or_else(|| "main".to_owned())
                })
            } else {
                package.clone()
            };
            default_exec_name(&import_path).to_owned()
        })
    }

    /// Build the `-o` argument for `go build`. When `dir` is set the child
    /// process runs in that subdirectory, so a relative `output` would land
    /// the artifact in the wrong place; this absolutises against `cwd` so it
    /// matches the bit-CWD-relative path that `resolve()` and
    /// `create_dir_all` use.
    fn output_arg(output: &str, dir_set: bool, cwd: &Path) -> String {
        if dir_set {
            cwd.join(output).to_string_lossy().into_owned()
        } else {
            output.to_owned()
        }
    }

    /// The arguments to `go` that build the binary to `output`.
    fn build_args(inputs: &GoExeInputs, output: &str) -> Vec<String> {
        let mut args = vec!["build".to_owned(), "-o".to_owned(), output.to_owned()];
        args.extend(inputs.flags.iter().cloned());
        args.push(inputs.package.clone());
        args
    }

    fn state_for(inputs: &GoExeInputs, output: String) -> GoExeState {
        GoExeState {
            package: inputs.package.clone(),
            output,
            flags: inputs.flags.clone(),
            env: inputs.env.clone(),
        }
    }
}

/// The last element of `import_path`, skipping a major version suffix such
/// as `/v2`, as `go build` names a binary.
fn default_exec_name(import_path: &str) -> &str {
    let import_path = import_path.trim_end_matches('/');
    let (parent, elem) = import_path.rsplit_once('/').unwrap_or(("", import_path));
    // `v0` and `v1` are never major version suffixes.
    let is_version = elem
        .strip_prefix('v')
        .is_some_and(|n| n != "1" && !n.starts_with('0') && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    if is_version && !parent.is_empty() {
        parent.rsplit('/').next().unwrap_or(parent)
    } else {
        elem
    }
}

impl Resource for GoExeResource {
    type State = GoExeState;
    type Inputs = GoExeInputs;
    type Outputs = GoExeOutputs;

    fn name(&self) -> &str {
        "exe"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn ambient(&self, inputs: &Block<GoExeInputs>) -> Vec<Ambient> {
        vec![super::toolchain(&inputs.env, inputs.dir.as_deref())]
    }

    fn resolve(&self, inputs: &Block<GoExeInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let tracker = &*self.tracker;
        let dir = inputs.dir.as_deref().map(Path::new);
        super::resolve_go_inputs(
            std::slice::from_ref(&inputs.package),
            dir,
            Tests::Excluded,
            &inputs.inputs,
            tracker,
        )
    }

    fn plan(&self, inputs: &Block<GoExeInputs>, prior_state: Option<&GoExeState>) -> Result<PlanResult, BoxError> {
        let output = GoExeResource::output_path(inputs);
        let description =
            super::display_command("go", &GoExeResource::build_args(inputs, &output), inputs.dir.as_deref());

        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description,
                reason: None,
            });
        };

        let action = if prior.package != inputs.package
            || prior.output != output
            || prior.flags != inputs.flags
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
        inputs: &Block<GoExeInputs>,
        _prior_state: Option<&GoExeState>,
        _changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<GoExeState, GoExeOutputs>, BoxError> {
        let output = GoExeResource::output_path(inputs);

        if let Some(parent) = Path::new(&output).parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }

        let cwd = std::env::current_dir()?;
        let output_arg = GoExeResource::output_arg(&output, inputs.dir.is_some(), &cwd);
        let args = GoExeResource::build_args(inputs, &output_arg);

        let mut cmd = Command::new("go");
        cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(dir) = &inputs.dir {
            cmd.current_dir(dir);
        }
        inputs.env.apply_to(&mut cmd);

        let mut child = cmd.spawn().map_err(|e| format!("failed to execute `go build`: {e}"))?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        std::thread::scope(|s| {
            if let Some(out) = stdout {
                s.spawn(|| writer.pipe_stdout(BufReader::new(out)));
            }
            if let Some(err) = stderr {
                s.spawn(|| writer.pipe_stderr(BufReader::new(err)));
            }
        });

        let status = child
            .wait()
            .map_err(|e| format!("failed to wait for `go build`: {e}"))?;
        if !status.success() {
            return Err(format!("`go build` exited with {status}").into());
        }

        Ok(ApplyResult {
            outputs: GoExeOutputs { path: output.clone() },
            state: Some(GoExeResource::state_for(inputs, output)),
        })
    }

    fn destroy(&self, prior_state: &Block<GoExeState>, writer: &BlockWriter) -> Result<(), BoxError> {
        crate::providers::remove_path(Path::new(&prior_state.output), writer)
    }

    /// Version 2 records the built binary under its project-relative path
    /// rather than a provider-defined role, so version 1 receipts are not
    /// readable.
    fn cache_policy(&self, _inputs: &Block<GoExeInputs>) -> CachePolicy {
        CachePolicy::Shared { version: 2 }
    }

    fn output_keys(&self, inputs: &Block<GoExeInputs>) -> Vec<String> {
        vec![GoExeResource::output_path(inputs)]
    }

    /// The default capture and check handle the binary itself. Only the state
    /// needs rebuilding: the receipt's came from another worktree, so it is
    /// re-derived from the current inputs.
    fn materialize(
        &self,
        inputs: &Block<GoExeInputs>,
        _state: &GoExeState,
        outputs: &[OutputFile],
        artifacts: &BTreeMap<String, ArtifactRef>,
        cas: &Cas,
        writer: &BlockWriter,
    ) -> MaterializeResult<GoExeState, GoExeOutputs> {
        restore_output_files(outputs, artifacts, cas, writer)?;
        let output = GoExeResource::output_path(inputs);
        Ok(Some(ApplyResult {
            outputs: GoExeOutputs { path: output.clone() },
            state: Some(GoExeResource::state_for(inputs, output)),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_resource() -> GoExeResource {
        GoExeResource::new(Arc::new(FileTracker::default()))
    }

    #[test]
    fn resource_kind_is_build() {
        let resource = test_resource();
        assert_eq!(Resource::kind(&resource), ResourceKind::Build);
    }

    #[test]
    fn resolve_tracks_extra_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("go.mod"), "module example.com/test\n").unwrap();
        std::fs::write(root.join("main.go"), "package main\n").unwrap();
        std::fs::write(root.join("main.c"), "int f(void) { return 0; }\n").unwrap();
        let inputs = Block::new(
            Some(root.to_string_lossy().into_owned()),
            GoExeInputs {
                package: ".".into(),
                output: None,
                flags: vec![],
                inputs: vec![root.join("*.c").to_string_lossy().into_owned()],
                env: GoEnv::default(),
            },
        );
        let resolved = Resource::resolve(&test_resource(), &inputs).unwrap();
        assert!(resolved.keys().any(|p| p.ends_with("main.c")), "{resolved:?}");
    }

    /// Pinned so that a change to how the binary is captured or restored is
    /// forced to bump the version rather than collide with older receipts.
    #[test]
    fn cache_version_is_pinned() {
        let inputs = Block::new(
            None,
            GoExeInputs {
                package: "./cmd/app".into(),
                output: Some("bin/app".into()),
                flags: vec![],
                inputs: vec![],
                env: GoEnv::default(),
            },
        );
        assert_eq!(
            Resource::cache_policy(&test_resource(), &inputs),
            CachePolicy::Shared { version: 2 }
        );
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = Block::new(
            None,
            GoExeInputs {
                package: "./cmd/app".into(),
                output: Some("bin/app".into()),
                flags: vec!["-trimpath".into()],
                inputs: vec![],
                env: GoEnv::default(),
            },
        );
        let resource = test_resource();
        let result = Resource::plan(&resource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "go build -o bin/app -trimpath ./cmd/app");
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = Block::new(
            None,
            GoExeInputs {
                package: "./cmd/app".into(),
                output: Some("bin/app".into()),
                flags: vec![],
                inputs: vec![],
                env: GoEnv::default(),
            },
        );
        let prior = GoExeState {
            package: "./cmd/app".into(),
            output: "bin/app".into(),
            flags: vec![],
            env: GoEnv::default(),
        };
        let resource = test_resource();
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn plan_update_when_package_changed() {
        let inputs = Block::new(
            None,
            GoExeInputs {
                package: "./cmd/other".into(),
                output: Some("bin/app".into()),
                flags: vec![],
                inputs: vec![],
                env: GoEnv::default(),
            },
        );
        let prior = GoExeState {
            package: "./cmd/app".into(),
            output: "bin/app".into(),
            flags: vec![],
            env: GoEnv::default(),
        };
        let resource = test_resource();
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn plan_update_when_flags_changed() {
        let inputs = Block::new(
            None,
            GoExeInputs {
                package: "./cmd/app".into(),
                output: Some("bin/app".into()),
                flags: vec!["-ldflags=-s".into()],
                inputs: vec![],
                env: GoEnv::default(),
            },
        );
        let prior = GoExeState {
            package: "./cmd/app".into(),
            output: "bin/app".into(),
            flags: vec![],
            env: GoEnv::default(),
        };
        let resource = test_resource();
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn plan_update_when_env_changed() {
        let inputs = Block::new(
            None,
            GoExeInputs {
                package: "./cmd/app".into(),
                output: Some("bin/app".into()),
                flags: vec![],
                inputs: vec![],
                env: GoEnv {
                    goos: Some("linux".into()),
                    goarch: Some("arm64".into()),
                    cgo: Some(false),
                },
            },
        );
        let prior = GoExeState {
            package: "./cmd/app".into(),
            output: "bin/app".into(),
            flags: vec![],
            env: GoEnv::default(),
        };
        let resource = test_resource();
        let result = Resource::plan(&resource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn output_path_default() {
        let inputs = Block::new(
            None,
            GoExeInputs {
                package: "./cmd/foo".into(),
                output: None,
                flags: vec![],
                inputs: vec![],
                env: GoEnv::default(),
            },
        );
        assert_eq!(GoExeResource::output_path(&inputs), "foo");
    }

    #[test]
    fn output_path_default_names_the_package_not_the_pattern() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "module example.com/app/v2\n").unwrap();
        std::fs::create_dir_all(dir.path().join("cmd/tool")).unwrap();
        let inputs = |package: &str| {
            Block::new(
                Some(dir.path().to_string_lossy().into_owned()),
                GoExeInputs {
                    package: package.into(),
                    output: None,
                    flags: vec![],
                    inputs: vec![],
                    env: GoEnv::default(),
                },
            )
        };
        assert_eq!(GoExeResource::output_path(&inputs(".")), "app");
        assert_eq!(GoExeResource::output_path(&inputs("./")), "app");
        assert_eq!(GoExeResource::output_path(&inputs("./cmd/tool/")), "tool");
        assert_eq!(
            GoExeResource::output_path(&inputs("example.com/app/v2/cmd/tool")),
            "tool"
        );
    }

    #[test]
    fn default_exec_name_skips_major_versions() {
        assert_eq!(default_exec_name("example.com/app/v2"), "app");
        assert_eq!(default_exec_name("example.com/app/v1"), "v1");
        assert_eq!(default_exec_name("example.com/app/v0"), "v0");
        assert_eq!(default_exec_name("v2"), "v2");
    }

    #[test]
    fn output_path_explicit() {
        let inputs = Block::new(
            None,
            GoExeInputs {
                package: "./cmd/foo".into(),
                output: Some("bin/foo".into()),
                flags: vec![],
                inputs: vec![],
                env: GoEnv::default(),
            },
        );
        assert_eq!(GoExeResource::output_path(&inputs), "bin/foo");
    }

    #[test]
    fn output_arg_relative_when_no_dir() {
        // Without `dir`, go runs in bit's CWD, so a relative path resolves
        // correctly. We pass it through unchanged.
        let arg = GoExeResource::output_arg("build/chromad", false, Path::new("/project"));
        assert_eq!(arg, "build/chromad");
    }

    #[test]
    fn output_arg_absolute_when_dir_set() {
        // With `dir`, go runs in the subdirectory, so we must absolutise the
        // output against bit's CWD or the binary lands in the wrong place.
        let arg = GoExeResource::output_arg("build/chromad", true, Path::new("/project"));
        assert_eq!(arg, "/project/build/chromad");
    }

    #[test]
    fn output_arg_preserves_absolute_path() {
        // An already-absolute output path is preserved as-is.
        let arg = GoExeResource::output_arg("/tmp/out/bin", true, Path::new("/project"));
        assert_eq!(arg, "/tmp/out/bin");
    }

    #[test]
    fn destroy_removes_output() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("mybin");
        std::fs::write(&output, "binary").unwrap();

        let state = GoExeState {
            package: "./cmd/app".into(),
            output: output.to_string_lossy().into_owned(),
            flags: vec![],
            env: GoEnv::default(),
        };

        let resource = test_resource();
        let out = crate::output::Output::new(&[]);
        let writer = out.writer("test");
        Resource::destroy(&resource, &Block::new(None, state), &writer).unwrap();
        assert!(!output.exists());
    }
}
