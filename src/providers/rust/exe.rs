use std::io::{BufRead, BufReader};
use std::process::Stdio;

use serde::{Deserialize, Serialize};

use crate::output::BlockWriter;
use crate::provider::{ApplyResult, BoxError, PlanAction, PlanResult, ResolvedFile, Resource, ResourceKind};

use super::{CargoCommand, RustEnv, RustFeatures};

/// Build a Rust binary
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct RustExeInputs {
    /// Binary target name (inferred if omitted)
    #[serde(default)]
    pub bin: Option<String>,
    /// Package containing the binary (-p flag)
    #[serde(default)]
    pub package: Option<String>,
    /// Extra flags passed to cargo build
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub features: RustFeatures,
    #[serde(flatten)]
    pub env: RustEnv,
}

/// Outputs from a `rust.exe` block.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct RustExeOutputs {
    /// Path to the built binary
    pub path: String,
}

/// Persisted state for a `rust.exe` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustExeState {
    pub bin: Option<String>,
    pub package: Option<String>,
    pub path: String,
    pub flags: Vec<String>,
    #[serde(flatten)]
    pub features: RustFeatures,
    #[serde(flatten)]
    pub env: RustEnv,
}

pub struct RustExeResource;

fn exe_command(inputs: &RustExeInputs) -> CargoCommand {
    let mut cargo = inputs.env.cargo("build");
    if let Some(bin) = &inputs.bin {
        cargo.arg2("--bin", bin);
    }
    if let Some(pkg) = &inputs.package {
        cargo.arg2("-p", pkg);
    }
    cargo.features(&inputs.features).extra_flags(&inputs.flags);
    cargo
}

/// Parse cargo's `--message-format=json` output, forwarding compiler
/// diagnostics to the writer and returning the path to the built binary.
fn find_binary_from_json(reader: impl BufRead, writer: &BlockWriter) -> Option<String> {
    let mut binary_path = None;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        // Capture the executable path from compiler-artifact messages.
        if msg.get("reason").and_then(|r| r.as_str()) == Some("compiler-artifact")
            && let Some(target) = msg.get("target")
            && target
                .get("kind")
                .and_then(|k| k.as_array())
                .is_some_and(|kinds| kinds.iter().any(|k| k.as_str() == Some("bin")))
            && let Some(executable) = msg.get("executable").and_then(|e| e.as_str())
        {
            binary_path = Some(executable.to_owned());
        }
        // Forward compiler diagnostics (warnings, errors).
        if let Some(rendered) = msg
            .get("message")
            .and_then(|m| m.get("rendered"))
            .and_then(|r| r.as_str())
        {
            writer.stderr_line(rendered);
        }
    }
    binary_path
}

impl Resource for RustExeResource {
    type State = RustExeState;
    type Inputs = RustExeInputs;
    type Outputs = RustExeOutputs;

    fn name(&self) -> &str {
        "exe"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn resolve(&self, _inputs: &RustExeInputs) -> Result<Vec<ResolvedFile>, BoxError> {
        super::resolve_rust_inputs()
    }

    fn plan(&self, inputs: &RustExeInputs, prior_state: Option<&RustExeState>) -> Result<PlanResult, BoxError> {
        let description = exe_command(inputs).display();

        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description,
                reason: None,
            });
        };

        let action = if prior.bin != inputs.bin
            || prior.package != inputs.package
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
        inputs: &RustExeInputs,
        _prior_state: Option<&RustExeState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<RustExeState, RustExeOutputs>, BoxError> {
        let mut cargo = exe_command(inputs);
        cargo.arg2("--message-format", "json");
        let mut child = cargo
            .command()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to execute `{}`: {e}", cargo.display()))?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let mut built_binary: Option<String> = None;
        std::thread::scope(|s| {
            let bin_handle = stdout.map(|out| s.spawn(move || find_binary_from_json(BufReader::new(out), writer)));
            if let Some(err) = stderr {
                s.spawn(|| writer.pipe_stderr(BufReader::new(err)));
            }
            if let Some(handle) = bin_handle {
                built_binary = handle.join().unwrap_or(None);
            }
        });

        let status = child
            .wait()
            .map_err(|e| format!("failed to wait for `{}`: {e}", cargo.display()))?;
        if !status.success() {
            return Err(format!("`{}` exited with {status}", cargo.display()).into());
        }

        let path = built_binary.ok_or("cargo build succeeded but no binary was produced")?;

        Ok(ApplyResult {
            outputs: RustExeOutputs { path: path.clone() },
            state: Some(RustExeState {
                bin: inputs.bin.clone(),
                package: inputs.package.clone(),
                path,
                flags: inputs.flags.clone(),
                features: inputs.features.clone(),
                env: inputs.env.clone(),
            }),
        })
    }

    fn destroy(&self, _prior_state: &RustExeState, _writer: &BlockWriter) -> Result<(), BoxError> {
        Ok(())
    }

    fn refresh(&self, prior_state: &RustExeState) -> Result<ApplyResult<RustExeState, RustExeOutputs>, BoxError> {
        Ok(ApplyResult {
            outputs: RustExeOutputs {
                path: prior_state.path.clone(),
            },
            state: Some(prior_state.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Output;

    #[test]
    fn resource_kind_is_build() {
        assert_eq!(Resource::kind(&RustExeResource), ResourceKind::Build);
    }

    #[test]
    fn plan_create_when_no_prior_state() {
        let inputs = RustExeInputs {
            bin: Some("myapp".into()),
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustExeResource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "cargo build --bin myapp");
    }

    #[test]
    fn plan_create_no_bin() {
        let inputs = RustExeInputs {
            bin: None,
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustExeResource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.description, "cargo build");
    }

    #[test]
    fn plan_create_with_package() {
        let inputs = RustExeInputs {
            bin: None,
            package: Some("my-crate".into()),
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustExeResource, &inputs, None).unwrap();
        assert_eq!(result.description, "cargo build -p my-crate");
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = RustExeInputs {
            bin: Some("myapp".into()),
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let prior = RustExeState {
            bin: Some("myapp".into()),
            package: None,
            path: "target/debug/myapp".into(),
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustExeResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn plan_update_when_bin_changed() {
        let inputs = RustExeInputs {
            bin: Some("other".into()),
            package: None,
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let prior = RustExeState {
            bin: Some("myapp".into()),
            package: None,
            path: "target/debug/myapp".into(),
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::plan(&RustExeResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn find_binary_from_json_output() {
        let json = concat!(
            r#"{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"myapp"},"executable":"/path/to/target/debug/myapp","filenames":["/path/to/target/debug/myapp"]}"#,
            "\n",
        );
        let out = Output::new(&[]);
        let writer = out.writer("test");
        let result = find_binary_from_json(std::io::Cursor::new(json), &writer);
        assert_eq!(result, Some("/path/to/target/debug/myapp".into()));
    }

    #[test]
    fn find_binary_skips_lib_artifacts() {
        let json = concat!(
            r#"{"reason":"compiler-artifact","target":{"kind":["lib"],"name":"mylib"},"filenames":["/path/to/target/debug/libmylib.rlib"]}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"myapp"},"executable":"/path/to/target/debug/myapp","filenames":["/path/to/target/debug/myapp"]}"#,
            "\n",
        );
        let out = Output::new(&[]);
        let writer = out.writer("test");
        let result = find_binary_from_json(std::io::Cursor::new(json), &writer);
        assert_eq!(result, Some("/path/to/target/debug/myapp".into()));
    }

    #[test]
    fn find_binary_returns_none_when_no_binary() {
        let json = concat!(
            r#"{"reason":"compiler-artifact","target":{"kind":["lib"],"name":"mylib"},"filenames":["/path/to/target/debug/libmylib.rlib"]}"#,
            "\n",
        );
        let out = Output::new(&[]);
        let writer = out.writer("test");
        let result = find_binary_from_json(std::io::Cursor::new(json), &writer);
        assert_eq!(result, None);
    }

    #[test]
    fn refresh_returns_outputs() {
        let state = RustExeState {
            bin: Some("myapp".into()),
            package: None,
            path: "target/debug/myapp".into(),
            flags: vec![],
            features: RustFeatures::default(),
            env: RustEnv::default(),
        };
        let result = Resource::refresh(&RustExeResource, &state).unwrap();
        assert_eq!(result.outputs.path, "target/debug/myapp");
    }
}
