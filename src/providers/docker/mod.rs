pub mod container;
pub mod image;
pub mod network;
pub mod parse;
pub mod push;

use crate::provider::{BoxError, DynResource, FuncSignature, Provider};
use crate::value::Value;

pub struct DockerProvider;

impl Provider for DockerProvider {
    fn name(&self) -> &str {
        "docker"
    }

    fn resources(&self) -> Vec<Box<dyn DynResource>> {
        vec![
            Box::new(image::ImageResource),
            Box::new(push::PushResource),
            Box::new(container::ContainerResource),
            Box::new(network::NetworkResource),
        ]
    }

    fn functions(&self) -> Vec<FuncSignature> {
        vec![]
    }

    fn call_function(&self, name: &str, _args: &[Value]) -> Result<Value, BoxError> {
        Err(format!("docker provider has no function '{name}'").into())
    }
}
