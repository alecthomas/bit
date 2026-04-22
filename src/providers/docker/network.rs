use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::{BlockWriter, Event};
use crate::provider::{ApplyResult, BoxError, PlanAction, PlanResult, Resource, ResourceKind};
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

pub struct NetworkResource;

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

    fn resolve(
        &self,
        _inputs: &NetworkInputs,
        _tracker: &mut FileTracker,
    ) -> Result<std::collections::BTreeMap<String, SHA256>, BoxError> {
        Ok(std::collections::BTreeMap::new())
    }

    fn plan(&self, inputs: &NetworkInputs, prior_state: Option<&NetworkState>) -> Result<PlanResult, BoxError> {
        let desc = format!("docker network create {}", inputs.name);

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
        cmd.arg("network").arg("create");
        if let Some(driver) = &inputs.driver {
            cmd.arg("--driver").arg(driver);
        }
        cmd.arg(&inputs.name);

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
        if network_id(&prior_state.name).is_none() {
            return Ok(());
        }
        writer.event(Event::Starting, &format!("docker network rm {}", prior_state.name));
        remove_network(&prior_state.name)
    }
}

fn remove_network(name: &str) -> Result<(), BoxError> {
    let out = Command::new("docker")
        .args(["network", "rm", name])
        .output()
        .map_err(|e| format!("docker network rm failed: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        // Ignore "not found" errors — idempotent destroy.
        if stderr.contains("not found") || stderr.contains("No such network") {
            return Ok(());
        }
        return Err(stderr.into());
    }
    Ok(())
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
        let result = Resource::plan(&NetworkResource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
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
        let result = Resource::plan(&NetworkResource, &inputs, Some(&prior)).unwrap();
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
