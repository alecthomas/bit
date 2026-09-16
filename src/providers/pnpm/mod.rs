pub mod install;
pub mod run;
pub mod test;
pub mod workspace;

use std::io::BufReader;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{BoxError, DynResource, FuncSignature, Provider};
use crate::value::Value;

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
        vec![]
    }

    fn call_function(&self, name: &str, _args: &[Value]) -> Result<Value, BoxError> {
        Err(format!("pnpm provider has no function '{name}'").into())
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
