pub mod container;
pub mod image;
pub mod network;
pub mod parse;
pub mod push;

use std::sync::{Arc, Mutex};

use crate::file_tracker::FileTracker;
use crate::provider::{BoxError, DynResource, FuncSignature, Provider};
use crate::value::Value;

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
        ]
    }

    fn functions(&self) -> Vec<FuncSignature> {
        vec![]
    }

    fn call_function(&self, name: &str, _args: &[Value]) -> Result<Value, BoxError> {
        Err(format!("docker provider has no function '{name}'").into())
    }
}
