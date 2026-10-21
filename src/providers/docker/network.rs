use std::process::Command;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::{BlockWriter, Event};
use crate::provider::{ApplyResult, BoxError, CachePolicy, Changes, PlanAction, PlanResult, Resource, ResourceKind};
use crate::sha256::SHA256;

/// Create a Docker network (Terraform-style: tracked state, drift detection)
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct NetworkInputs {
    /// Network name (must be unique per daemon)
    pub name: String,
    /// Network driver (bridge, host, overlay, ...). Defaults to `bridge`.
    #[serde(default)]
    pub driver: Option<String>,
}

#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct NetworkOutputs {
    /// Network name
    pub name: String,
    /// Docker network ID
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkState {
    pub name: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
}

/// Query `docker network inspect <name>` and return the network ID if it exists.
fn network_id(name: &str) -> Option<String> {
    let out = Command::new("docker")
        .args(["network", "inspect", "--format", "{{.Id}}", name])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!id.is_empty()).then_some(id)
}

/// The arguments to `docker` that create the block's network.
fn create_args(inputs: &NetworkInputs) -> Vec<String> {
    let mut args = vec!["network".to_owned(), "create".to_owned()];
    if let Some(driver) = &inputs.driver {
        args.extend(["--driver".to_owned(), driver.clone()]);
    }
    args.push(inputs.name.clone());
    args
}

pub struct NetworkResource {
    #[allow(dead_code)]
    tracker: Arc<FileTracker>,
}

impl NetworkResource {
    pub(super) fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

impl Resource for NetworkResource {
    type State = NetworkState;
    type Inputs = NetworkInputs;
    type Outputs = NetworkOutputs;

    fn name(&self) -> &str {
        "network"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn resolve(&self, _inputs: &NetworkInputs) -> Result<std::collections::BTreeMap<String, SHA256>, BoxError> {
        Ok(std::collections::BTreeMap::new())
    }

    /// None: the network is daemon state that the docker CLI only requests,
    /// so a CLI upgrade must not recreate it under its attached containers.
    fn ambient(&self, _inputs: &NetworkInputs) -> Vec<crate::provider::Ambient> {
        vec![]
    }

    fn plan(&self, inputs: &NetworkInputs, prior_state: Option<&NetworkState>) -> Result<PlanResult, BoxError> {
        let desc = format!("docker {}", create_args(inputs).join(" "));

        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description: desc,
                reason: None,
            });
        };

        if network_id(&prior.name).is_none() {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description: desc,
                reason: Some("network missing".into()),
            });
        }

        if prior.name != inputs.name || prior.driver != inputs.driver {
            return Ok(PlanResult {
                action: PlanAction::Update,
                description: desc,
                reason: None,
            });
        }

        Ok(PlanResult {
            action: PlanAction::None,
            description: desc,
            reason: None,
        })
    }

    fn apply(
        &self,
        inputs: &NetworkInputs,
        prior_state: Option<&NetworkState>,
        _changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<NetworkState, NetworkOutputs>, BoxError> {
        // Adopt an existing network with the same name. If the driver
        // differs from what was requested, recreate.
        if let Some(id) = network_id(&inputs.name) {
            let driver_matches = match prior_state {
                Some(prior) => prior.driver == inputs.driver,
                None => true,
            };
            if driver_matches {
                return Ok(ApplyResult {
                    state: Some(NetworkState {
                        name: inputs.name.clone(),
                        id: id.clone(),
                        driver: inputs.driver.clone(),
                    }),
                    outputs: NetworkOutputs {
                        name: inputs.name.clone(),
                        id,
                    },
                });
            }
            writer.event(Event::Starting, &format!("docker network rm {}", inputs.name));
            remove_network(&inputs.name)?;
        }

        let mut cmd = Command::new("docker");
        cmd.args(create_args(inputs));

        let output = cmd.output().map_err(|e| format!("docker network create failed: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(stderr.into());
        }
        let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        writer.line(&format!("created network {} ({})", inputs.name, short_id(&id)));

        Ok(ApplyResult {
            state: Some(NetworkState {
                name: inputs.name.clone(),
                id: id.clone(),
                driver: inputs.driver.clone(),
            }),
            outputs: NetworkOutputs {
                name: inputs.name.clone(),
                id,
            },
        })
    }

    fn destroy(&self, prior_state: &NetworkState, writer: &BlockWriter) -> Result<(), BoxError> {
        writer.event(Event::Starting, &format!("docker network rm {}", prior_state.name));
        remove_network(&prior_state.name)
    }

    /// A network is named, mutable state in the local Docker daemon; see
    /// `docker.container`.
    fn cache_policy(&self, _inputs: &NetworkInputs) -> CachePolicy {
        CachePolicy::Local
    }
}

fn remove_network(name: &str) -> Result<(), BoxError> {
    let out = Command::new("docker")
        .args(["network", "rm", name])
        .output()
        .map_err(|e| format!("docker network rm failed: {e}"))?;
    super::check_remove_output("docker network rm", out)
}

fn short_id(id: &str) -> &str {
    let end = id.len().min(12);
    &id[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_create_when_no_state() {
        let inputs = NetworkInputs {
            name: "test-net".into(),
            driver: None,
        };
        let result = Resource::plan(&NetworkResource::new(Arc::new(FileTracker::default())), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn description_includes_driver() {
        let inputs = NetworkInputs {
            name: "test-net".into(),
            driver: Some("bridge".into()),
        };
        let result = Resource::plan(&NetworkResource::new(Arc::new(FileTracker::default())), &inputs, None).unwrap();
        assert_eq!(result.description, "docker network create --driver bridge test-net");
    }

    #[test]
    fn plan_create_when_network_missing() {
        let inputs = NetworkInputs {
            name: "bit-nonexistent-network-test".into(),
            driver: None,
        };
        let prior = NetworkState {
            name: "bit-nonexistent-network-test".into(),
            id: "abc".into(),
            driver: None,
        };
        let result = Resource::plan(
            &NetworkResource::new(Arc::new(FileTracker::default())),
            &inputs,
            Some(&prior),
        )
        .unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(result.reason.as_deref(), Some("network missing"));
    }

    #[test]
    fn short_id_truncates_long() {
        assert_eq!(short_id("0123456789abcdef0123"), "0123456789ab");
    }

    #[test]
    fn short_id_passes_through_short() {
        assert_eq!(short_id("abc"), "abc");
    }
}
