use std::collections::{BTreeMap, HashMap};
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::{BlockWriter, Event};
use crate::provider::{ApplyResult, BoxError, PlanAction, PlanResult, Resource, ResourceKind};
use crate::sha256::SHA256;

/// Attach a container to a Docker network (equivalent of `docker network connect`).
///
/// Mirrors every flag of the underlying CLI so this block fully replaces a
/// hand-rolled `exec` wrapper. The attachment is idempotent and is tracked
/// in state so drift detection works across runs.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, bit_derive::Schema)]
pub struct NetworkAttachInputs {
    /// Network name or ID
    pub network: String,
    /// Container name or ID
    pub container: String,
    /// Network-scoped aliases for the container (`--alias`)
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Driver options as key/value pairs (`--driver-opt`)
    #[serde(default)]
    pub driver_opts: HashMap<String, String>,
    /// Default-gateway priority on this endpoint (`--gw-priority`).
    /// Highest priority provides the default gateway; accepts negative values.
    #[serde(default)]
    pub gw_priority: Option<i32>,
    /// IPv4 address (`--ip`)
    #[serde(default)]
    pub ip: Option<String>,
    /// IPv6 address (`--ip6`)
    #[serde(default)]
    pub ip6: Option<String>,
    /// Links to other containers, in `name:alias` form (`--link`)
    #[serde(default)]
    pub links: Vec<String>,
    /// Link-local addresses for the container (`--link-local-ip`)
    #[serde(default)]
    pub link_local_ips: Vec<String>,
}

#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct NetworkAttachOutputs {
    /// IPv4 address Docker assigned to the endpoint, if any.
    pub ip_address: Option<String>,
    /// IPv6 address Docker assigned to the endpoint, if any.
    pub ip6_address: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkAttachState {
    pub network: String,
    pub container: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub driver_opts: HashMap<String, String>,
    #[serde(default)]
    pub gw_priority: Option<i32>,
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub ip6: Option<String>,
    #[serde(default)]
    pub links: Vec<String>,
    #[serde(default)]
    pub link_local_ips: Vec<String>,
}

impl NetworkAttachState {
    fn from_inputs(inputs: &NetworkAttachInputs) -> Self {
        Self {
            network: inputs.network.clone(),
            container: inputs.container.clone(),
            aliases: inputs.aliases.clone(),
            driver_opts: inputs.driver_opts.clone(),
            gw_priority: inputs.gw_priority,
            ip: inputs.ip.clone(),
            ip6: inputs.ip6.clone(),
            links: inputs.links.clone(),
            link_local_ips: inputs.link_local_ips.clone(),
        }
    }
}

/// True when the persisted state matches the current inputs verbatim.
fn state_matches_inputs(state: &NetworkAttachState, inputs: &NetworkAttachInputs) -> bool {
    *state == NetworkAttachState::from_inputs(inputs)
}

/// Run `docker inspect <container>` and return the `NetworkSettings.Networks`
/// object as raw JSON text.
fn inspect_networks(container: &str) -> Option<String> {
    let out = Command::new("docker")
        .args(["inspect", container, "--format", "{{json .NetworkSettings.Networks}}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Check whether `container` is currently attached to `network`. Returns
/// `false` on any error (missing container, daemon unreachable,
/// unparseable JSON) — callers treat that as "needs reconnect".
fn is_attached(network: &str, container: &str) -> bool {
    let Some(json_text) = inspect_networks(container) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&json_text) else {
        return false;
    };
    json.as_object().is_some_and(|o| o.contains_key(network))
}

/// Extract the IPv4 and IPv6 addresses Docker assigned for this endpoint
/// out of the `NetworkSettings.Networks` JSON. Empty-string IP fields
/// (Docker's "unset" representation) become `None`.
fn parse_endpoint_ips(networks_json: &str, network: &str) -> Result<(Option<String>, Option<String>), BoxError> {
    let json: serde_json::Value = serde_json::from_str(networks_json)?;
    let entry = json
        .get(network)
        .ok_or_else(|| format!("container is not attached to network '{network}'"))?;
    let pick = |key: &str| {
        entry
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    Ok((pick("IPAddress"), pick("GlobalIPv6Address")))
}

/// Query Docker for the assigned endpoint addresses on this attachment.
fn read_endpoint_ips(container: &str, network: &str) -> Result<(Option<String>, Option<String>), BoxError> {
    let json_text = inspect_networks(container).ok_or_else(|| format!("docker inspect {container} failed"))?;
    parse_endpoint_ips(&json_text, network)
}

/// Build the `docker network connect` argv. Repeated flags (aliases,
/// driver-opts, links, link-local-ips) are sorted so that the command
/// line is deterministic for tests and log diffs.
fn build_connect_args(inputs: &NetworkAttachInputs) -> Vec<String> {
    let mut args: Vec<String> = vec!["network".into(), "connect".into()];

    let mut aliases = inputs.aliases.clone();
    aliases.sort();
    for alias in aliases {
        args.push("--alias".into());
        args.push(alias);
    }

    let mut opts: Vec<(&String, &String)> = inputs.driver_opts.iter().collect();
    opts.sort_by(|a, b| a.0.cmp(b.0));
    for (k, v) in opts {
        args.push("--driver-opt".into());
        args.push(format!("{k}={v}"));
    }

    if let Some(p) = inputs.gw_priority {
        args.push("--gw-priority".into());
        args.push(p.to_string());
    }
    if let Some(ip) = &inputs.ip {
        args.push("--ip".into());
        args.push(ip.clone());
    }
    if let Some(ip6) = &inputs.ip6 {
        args.push("--ip6".into());
        args.push(ip6.clone());
    }

    let mut links = inputs.links.clone();
    links.sort();
    for link in links {
        args.push("--link".into());
        args.push(link);
    }

    let mut link_local_ips = inputs.link_local_ips.clone();
    link_local_ips.sort();
    for addr in link_local_ips {
        args.push("--link-local-ip".into());
        args.push(addr);
    }

    args.push(inputs.network.clone());
    args.push(inputs.container.clone());
    args
}

/// Run `docker network connect`. Already-attached is treated as success.
fn run_connect(inputs: &NetworkAttachInputs) -> Result<(), BoxError> {
    let out = Command::new("docker")
        .args(build_connect_args(inputs))
        .output()
        .map_err(|e| format!("docker network connect failed: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    if stderr.contains("already exists") || stderr.contains("is already connected to") {
        return Ok(());
    }
    Err(stderr.into())
}

/// Run `docker network disconnect`. Not-attached / missing-network /
/// missing-container are treated as success so destroy is idempotent.
fn run_disconnect(network: &str, container: &str) -> Result<(), BoxError> {
    let out = Command::new("docker")
        .args(["network", "disconnect", network, container])
        .output()
        .map_err(|e| format!("docker network disconnect failed: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    if stderr.contains("is not connected to network")
        || stderr.contains("not connected")
        || stderr.contains("No such")
        || stderr.contains("not found")
    {
        return Ok(());
    }
    Err(stderr.into())
}

pub struct NetworkAttachResource {
    #[allow(dead_code)]
    tracker: Arc<Mutex<FileTracker>>,
}

impl NetworkAttachResource {
    pub(super) fn new(tracker: Arc<Mutex<FileTracker>>) -> Self {
        Self { tracker }
    }
}

impl Resource for NetworkAttachResource {
    type State = NetworkAttachState;
    type Inputs = NetworkAttachInputs;
    type Outputs = NetworkAttachOutputs;

    fn name(&self) -> &str {
        "network_attach"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn resolve(&self, _inputs: &NetworkAttachInputs) -> Result<BTreeMap<String, SHA256>, BoxError> {
        Ok(BTreeMap::new())
    }

    fn plan(
        &self,
        inputs: &NetworkAttachInputs,
        prior_state: Option<&NetworkAttachState>,
    ) -> Result<PlanResult, BoxError> {
        let desc = format!("docker network connect {} {}", inputs.network, inputs.container);

        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description: desc,
                reason: None,
            });
        };

        if !is_attached(&inputs.network, &inputs.container) {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description: desc,
                reason: Some("endpoint missing".into()),
            });
        }

        if !state_matches_inputs(prior, inputs) {
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
        inputs: &NetworkAttachInputs,
        prior_state: Option<&NetworkAttachState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<NetworkAttachState, NetworkAttachOutputs>, BoxError> {
        // Endpoint options (aliases, ip, links, ...) can't be changed in
        // place — Docker only honours them at connect time. If we have any
        // prior attachment (current or under a different network/container
        // name from a config edit), disconnect first so the reconnect
        // produces a clean endpoint matching the current inputs.
        if let Some(prior) = prior_state {
            run_disconnect(&prior.network, &prior.container)?;
        }
        // Also clean up any external attachment under the *current* names
        // (e.g. someone connected the container by hand before us).
        if is_attached(&inputs.network, &inputs.container) {
            run_disconnect(&inputs.network, &inputs.container)?;
        }

        writer.event(
            Event::Starting,
            &format!("docker network connect {} {}", inputs.network, inputs.container),
        );
        run_connect(inputs)?;

        let (ip_address, ip6_address) = read_endpoint_ips(&inputs.container, &inputs.network)?;

        Ok(ApplyResult {
            state: Some(NetworkAttachState::from_inputs(inputs)),
            outputs: NetworkAttachOutputs {
                ip_address,
                ip6_address,
            },
        })
    }

    fn destroy(&self, prior_state: &NetworkAttachState, writer: &BlockWriter) -> Result<(), BoxError> {
        writer.event(
            Event::Starting,
            &format!(
                "docker network disconnect {} {}",
                prior_state.network, prior_state.container
            ),
        );
        run_disconnect(&prior_state.network, &prior_state.container)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::PlanAction;

    fn base_inputs() -> NetworkAttachInputs {
        NetworkAttachInputs {
            network: "bit-test-net".into(),
            container: "bit-test-container".into(),
            aliases: vec![],
            driver_opts: HashMap::new(),
            gw_priority: None,
            ip: None,
            ip6: None,
            links: vec![],
            link_local_ips: vec![],
        }
    }

    #[test]
    fn build_connect_args_minimal() {
        let args = build_connect_args(&base_inputs());
        assert_eq!(args, vec!["network", "connect", "bit-test-net", "bit-test-container"]);
    }

    #[test]
    fn build_connect_args_full_and_sorted() {
        let mut inputs = base_inputs();
        inputs.aliases = vec!["beta".into(), "alpha".into()];
        inputs.driver_opts.insert("b.opt".into(), "2".into());
        inputs.driver_opts.insert("a.opt".into(), "1".into());
        inputs.gw_priority = Some(-3);
        inputs.ip = Some("10.0.0.5".into());
        inputs.ip6 = Some("2001:db8::5".into());
        inputs.links = vec!["other:alias".into()];
        inputs.link_local_ips = vec!["169.254.0.2".into(), "169.254.0.1".into()];

        let args = build_connect_args(&inputs);
        assert_eq!(
            args,
            vec![
                "network",
                "connect",
                "--alias",
                "alpha",
                "--alias",
                "beta",
                "--driver-opt",
                "a.opt=1",
                "--driver-opt",
                "b.opt=2",
                "--gw-priority",
                "-3",
                "--ip",
                "10.0.0.5",
                "--ip6",
                "2001:db8::5",
                "--link",
                "other:alias",
                "--link-local-ip",
                "169.254.0.1",
                "--link-local-ip",
                "169.254.0.2",
                "bit-test-net",
                "bit-test-container",
            ]
        );
    }

    fn fresh_resource() -> NetworkAttachResource {
        NetworkAttachResource::new(Arc::new(Mutex::new(FileTracker::default())))
    }

    #[test]
    fn plan_create_when_no_state() {
        let plan = Resource::plan(&fresh_resource(), &base_inputs(), None).unwrap();
        assert_eq!(plan.action, PlanAction::Create);
    }

    #[test]
    fn plan_create_when_endpoint_missing() {
        // Using clearly non-existent names so `docker inspect` fails (or, if
        // docker is unavailable on the test host, the error path also
        // yields `is_attached == false`).
        let inputs = NetworkAttachInputs {
            network: "bit-nonexistent-net-zzz".into(),
            container: "bit-nonexistent-container-zzz".into(),
            ..base_inputs()
        };
        let prior = NetworkAttachState::from_inputs(&inputs);
        let plan = Resource::plan(&fresh_resource(), &inputs, Some(&prior)).unwrap();
        assert_eq!(plan.action, PlanAction::Create);
        assert_eq!(plan.reason.as_deref(), Some("endpoint missing"));
    }

    #[test]
    fn state_matches_detects_alias_drift() {
        let mut inputs = base_inputs();
        let prior = NetworkAttachState::from_inputs(&inputs);
        inputs.aliases.push("new-alias".into());
        assert!(!state_matches_inputs(&prior, &inputs));
    }

    #[test]
    fn state_matches_detects_ip_drift() {
        let mut inputs = base_inputs();
        let prior = NetworkAttachState::from_inputs(&inputs);
        inputs.ip = Some("10.0.0.7".into());
        assert!(!state_matches_inputs(&prior, &inputs));
    }

    #[test]
    fn state_matches_round_trip() {
        let inputs = base_inputs();
        let prior = NetworkAttachState::from_inputs(&inputs);
        assert!(state_matches_inputs(&prior, &inputs));
    }

    #[test]
    fn parse_endpoint_ips_both_addresses() {
        let json = r#"{
            "k3d-example": {
                "IPAddress": "192.168.215.3",
                "GlobalIPv6Address": "2001:db8::5"
            }
        }"#;
        let (v4, v6) = parse_endpoint_ips(json, "k3d-example").unwrap();
        assert_eq!(v4.as_deref(), Some("192.168.215.3"));
        assert_eq!(v6.as_deref(), Some("2001:db8::5"));
    }

    #[test]
    fn parse_endpoint_ips_empty_strings_become_none() {
        let json = r#"{
            "bridge": {
                "IPAddress": "10.0.0.5",
                "GlobalIPv6Address": ""
            }
        }"#;
        let (v4, v6) = parse_endpoint_ips(json, "bridge").unwrap();
        assert_eq!(v4.as_deref(), Some("10.0.0.5"));
        assert!(v6.is_none());
    }

    #[test]
    fn parse_endpoint_ips_missing_network_errors() {
        let json = r#"{ "bridge": { "IPAddress": "10.0.0.5" } }"#;
        let err = parse_endpoint_ips(json, "absent").unwrap_err();
        assert!(err.to_string().contains("not attached"));
    }
}
