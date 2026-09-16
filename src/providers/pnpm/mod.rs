pub mod install;
pub mod run;
pub mod test;
pub mod workspace;

use std::io::BufReader;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{BoxError, DynResource, FuncSignature, Provider, StructField};
use crate::value::{Type, Value};

fn packages_with_script(args: &[Value]) -> Result<Value, BoxError> {
    if !(1..=2).contains(&args.len()) {
        return Err(format!("pnpm.packages_with_script expects 1 or 2 arguments, got {}", args.len()).into());
    }
    let script = args[0]
        .as_str()
        .ok_or("pnpm.packages_with_script script must be a string")?;
    let dir = args
        .get(1)
        .map(|value| value.as_str().ok_or("pnpm.packages_with_script dir must be a string"))
        .transpose()?
        .unwrap_or(".");
    let packages = workspace::with_workspace(std::path::Path::new(dir), |workspace| {
        workspace::packages_with_script(workspace, script)
    })?;
    Ok(Value::List(
        Type::String,
        packages.into_iter().map(Value::Str).collect(),
    ))
}

/// pnpm-aware provider with `install`, `run`, and `test` resources.
pub struct PnpmProvider {
    tracker: Arc<Mutex<FileTracker>>,
}

impl PnpmProvider {
    pub fn new(tracker: Arc<Mutex<FileTracker>>) -> Self {
        Self { tracker }
    }
}

impl Provider for PnpmProvider {
    fn name(&self) -> &str {
        "pnpm"
    }

    fn resources(&self) -> Vec<Box<dyn DynResource>> {
        vec![
            Box::new(install::PnpmInstallResource {
                tracker: self.tracker.clone(),
            }),
            Box::new(run::PnpmRunResource {
                tracker: self.tracker.clone(),
            }),
            Box::new(test::PnpmTestResource {
                tracker: self.tracker.clone(),
            }),
        ]
    }

    fn functions(&self) -> Vec<FuncSignature> {
        vec![FuncSignature {
            name: "packages_with_script".into(),
            params: vec![
                (
                    "script".into(),
                    StructField {
                        typ: Type::String,
                        default: None,
                        description: Some("Script that each returned package must define".into()),
                    },
                ),
                (
                    "dir".into(),
                    StructField {
                        typ: Type::String,
                        default: Some(Value::Str(".".into())),
                        description: Some("Workspace root directory".into()),
                    },
                ),
            ],
            returns: Type::List(Box::new(Type::String)),
        }]
    }

    fn call_function(&self, name: &str, args: &[Value]) -> Result<Value, BoxError> {
        match name {
            "packages_with_script" => packages_with_script(args),
            _ => Err(format!("pnpm provider has no function '{name}'").into()),
        }
    }
}

/// Run `pnpm <args...>`, streaming stdout/stderr through the block writer.
/// Returns `Err` on non-zero exit. Used by all three pnpm resources.
pub(crate) fn run_pnpm(args: &[String], dir: Option<&str>, writer: &BlockWriter) -> Result<(), BoxError> {
    let mut cmd = Command::new("pnpm");
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(d) = dir {
        cmd.current_dir(d);
    }

    let mut child = cmd.spawn().map_err(|e| format!("failed to execute `pnpm`: {e}"))?;
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

    let status = child.wait().map_err(|e| format!("failed to wait for `pnpm`: {e}"))?;
    if !status.success() {
        return Err(format!("`pnpm` exited with {status}").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_registration() {
        let provider = PnpmProvider::new(Arc::new(Mutex::new(FileTracker::new())));
        assert_eq!(provider.name(), "pnpm");
        let resources = provider.resources();
        assert_eq!(resources.len(), 3);
        assert_eq!(resources[0].name(), "install");
        assert_eq!(resources[1].name(), "run");
        assert_eq!(resources[2].name(), "test");
    }
}
