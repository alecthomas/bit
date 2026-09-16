pub mod container;
pub mod image;
pub mod network;
pub mod network_attach;
pub mod parse;
pub mod push;

use std::process::Output;
use std::sync::{Arc, Mutex};

use crate::file_tracker::FileTracker;
use crate::provider::{BoxError, DynResource, FuncSignature, Provider};
use crate::value::Value;

/// Validate a Docker removal command. Destroy is idempotent when the object
/// has already gone, but other daemon and command failures must be reported.
pub(super) fn check_remove_output(action: &str, output: Output) -> Result<(), BoxError> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("no such") || lower.contains("not found") {
        return Ok(());
    }
    Err(format!("{action} failed: {stderr}").into())
}

pub struct DockerProvider {
    tracker: Arc<Mutex<FileTracker>>,
}

impl DockerProvider {
    pub fn new(tracker: Arc<Mutex<FileTracker>>) -> Self {
        Self { tracker }
    }
}

impl Provider for DockerProvider {
    fn name(&self) -> &str {
        "docker"
    }

    fn resources(&self) -> Vec<Box<dyn DynResource>> {
        vec![
            Box::new(image::ImageResource::new(self.tracker.clone())),
            Box::new(push::PushResource::new(self.tracker.clone())),
            Box::new(container::ContainerResource::new(self.tracker.clone())),
            Box::new(network::NetworkResource::new(self.tracker.clone())),
            Box::new(network_attach::NetworkAttachResource::new(self.tracker.clone())),
        ]
    }

    fn functions(&self) -> Vec<FuncSignature> {
        vec![]
    }

    fn call_function(&self, name: &str, _args: &[Value]) -> Result<Value, BoxError> {
        Err(format!("docker provider has no function '{name}'").into())
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::check_remove_output;

    #[test]
    fn removal_treats_missing_object_as_success() {
        let output = Command::new("sh")
            .args(["-c", "echo 'Error: No such image: missing' >&2; exit 1"])
            .output()
            .unwrap();
        check_remove_output("docker rmi", output).unwrap();
    }

    #[test]
    fn removal_propagates_other_failures() {
        let output = Command::new("sh")
            .args(["-c", "echo 'permission denied' >&2; exit 1"])
            .output()
            .unwrap();
        let error = check_remove_output("docker rmi", output).unwrap_err();
        assert!(error.to_string().contains("permission denied"));
    }
}
