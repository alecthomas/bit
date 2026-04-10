use std::collections::HashMap;
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::output::BlockWriter;
use crate::provider::{ApplyResult, BoxError, PlanAction, PlanResult, Resource, ResourceKind};

/// Healthcheck config — either a bare command string or a full object
/// with interval/timeout/retries/start_period.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Healthcheck {
    Command(String),
    Full {
        test: String,
        #[serde(default = "default_interval")]
        interval: String,
        #[serde(default = "default_timeout")]
        timeout: String,
        #[serde(default = "default_retries")]
        retries: u32,
        #[serde(default)]
        start_period: Option<String>,
    },
}

impl Healthcheck {
    fn test(&self) -> &str {
        match self {
            Healthcheck::Command(cmd) => cmd,
            Healthcheck::Full { test, .. } => test,
        }
    }

    fn interval(&self) -> &str {
        match self {
            Healthcheck::Command(_) => "5s",
            Healthcheck::Full { interval, .. } => interval,
        }
    }

    fn timeout(&self) -> &str {
        match self {
            Healthcheck::Command(_) => "5s",
            Healthcheck::Full { timeout, .. } => timeout,
        }
    }

    fn retries(&self) -> u32 {
        match self {
            Healthcheck::Command(_) => 3,
            Healthcheck::Full { retries, .. } => *retries,
        }
    }

    fn start_period(&self) -> Option<&str> {
        match self {
            Healthcheck::Command(_) => None,
            Healthcheck::Full { start_period, .. } => start_period.as_deref(),
        }
    }

    /// Total deadline: start_period + (interval * retries) + timeout.
    fn deadline(&self) -> Duration {
        let start = self.start_period().map(parse_duration).unwrap_or_default();
        let interval = parse_duration(self.interval());
        let timeout = parse_duration(self.timeout());
        start + interval * self.retries() + timeout
    }

    fn poll_interval(&self) -> Duration {
        parse_duration(self.interval())
    }
}

fn default_interval() -> String {
    "5s".into()
}

fn default_timeout() -> String {
    "5s".into()
}

fn default_retries() -> u32 {
    3
}

/// Parse a Docker-style duration string (e.g. "10s", "1m", "500ms").
fn parse_duration(s: &str) -> Duration {
    let s = s.trim();
    if let Some(ms) = s.strip_suffix("ms") {
        return Duration::from_millis(ms.parse().unwrap_or(0));
    }
    if let Some(m) = s.strip_suffix('m') {
        return Duration::from_secs(m.parse::<u64>().unwrap_or(0) * 60);
    }
    if let Some(secs) = s.strip_suffix('s') {
        return Duration::from_secs(secs.parse().unwrap_or(0));
    }
    // Bare number treated as seconds
    Duration::from_secs(s.parse().unwrap_or(0))
}

/// Hash a healthcheck config for change detection.
fn hash_healthcheck(hasher: &mut Sha256, hc: &Healthcheck) {
    hasher.update(b"healthcheck\0");
    hasher.update(hc.test().as_bytes());
    hasher.update(b"\0");
    hasher.update(hc.interval().as_bytes());
    hasher.update(b"\0");
    hasher.update(hc.timeout().as_bytes());
    hasher.update(b"\0");
    hasher.update(hc.retries().to_string().as_bytes());
    hasher.update(b"\0");
    if let Some(sp) = hc.start_period() {
        hasher.update(sp.as_bytes());
    }
    hasher.update(b"\0");
}

#[derive(Debug, Deserialize)]
pub struct ContainerInputs {
    pub image: String,
    pub name: String,
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(default)]
    pub volumes: Vec<String>,
    #[serde(default)]
    pub environment: HashMap<String, String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub entrypoint: Option<String>,
    #[serde(default = "default_restart")]
    pub restart: String,
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub healthcheck: Option<Healthcheck>,
}

fn default_restart() -> String {
    "no".into()
}

/// Compute a deterministic hash of all container config fields so we can
/// detect when the container needs to be recreated.
fn config_hash(inputs: &ContainerInputs) -> String {
    let mut hasher = Sha256::new();
    hasher.update(inputs.image.as_bytes());
    hasher.update(b"\0");
    hasher.update(inputs.name.as_bytes());
    hasher.update(b"\0");
    for p in &inputs.ports {
        hasher.update(p.as_bytes());
        hasher.update(b"\0");
    }
    for v in &inputs.volumes {
        hasher.update(v.as_bytes());
        hasher.update(b"\0");
    }
    // Sort env keys for determinism
    let mut env_keys: Vec<&String> = inputs.environment.keys().collect();
    env_keys.sort();
    for k in env_keys {
        hasher.update(k.as_bytes());
        hasher.update(b"=");
        hasher.update(inputs.environment[k].as_bytes());
        hasher.update(b"\0");
    }
    if let Some(cmd) = &inputs.command {
        hasher.update(cmd.as_bytes());
    }
    hasher.update(b"\0");
    if let Some(ep) = &inputs.entrypoint {
        hasher.update(ep.as_bytes());
    }
    hasher.update(b"\0");
    hasher.update(inputs.restart.as_bytes());
    hasher.update(b"\0");
    if let Some(net) = &inputs.network {
        hasher.update(net.as_bytes());
    }
    hasher.update(b"\0");
    if let Some(wd) = &inputs.working_dir {
        hasher.update(wd.as_bytes());
    }
    hasher.update(b"\0");
    if let Some(hc) = &inputs.healthcheck {
        hash_healthcheck(&mut hasher, hc);
    }
    format!("{:x}", hasher.finalize())
}

#[derive(Debug, Serialize)]
pub struct ContainerOutputs {
    pub container_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerState {
    pub name: String,
    pub container_id: String,
    pub config_hash: String,
}

/// Check whether a container with the given name is running.
fn container_running(name: &str) -> bool {
    Command::new("docker")
        .args(["inspect", "--format", "{{.State.Running}}", name])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false)
}

/// Remove a container by name (forced).
fn remove_container(name: &str) -> Result<(), BoxError> {
    let output = Command::new("docker")
        .args(["rm", "-f", name])
        .output()
        .map_err(|e| format!("docker rm failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(stderr.into());
    }
    Ok(())
}

/// Poll `docker inspect` until the container's health status is "healthy"
/// or the deadline is exceeded.
fn wait_healthy(name: &str, hc: &Healthcheck, writer: &BlockWriter) -> Result<(), BoxError> {
    let deadline = hc.deadline();
    let interval = hc.poll_interval();
    let start = Instant::now();

    writer.line("waiting for healthcheck...");

    loop {
        std::thread::sleep(interval);

        let output = Command::new("docker")
            .args(["inspect", "--format", "{{.State.Health.Status}}", name])
            .output()
            .map_err(|e| format!("docker inspect failed: {e}"))?;

        let status = String::from_utf8_lossy(&output.stdout).trim().to_owned();

        match status.as_str() {
            "healthy" => {
                writer.line("healthy");
                return Ok(());
            }
            "unhealthy" => {
                return Err("container healthcheck failed: unhealthy".into());
            }
            _ => {
                if start.elapsed() > deadline {
                    return Err(format!(
                        "container healthcheck timed out after {}s",
                        deadline.as_secs()
                    )
                    .into());
                }
            }
        }
    }
}

pub struct ContainerResource;

impl Resource for ContainerResource {
    type State = ContainerState;
    type Inputs = ContainerInputs;
    type Outputs = ContainerOutputs;

    fn name(&self) -> &str {
        "container"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn resolve(&self, _inputs: &ContainerInputs) -> Result<Vec<crate::provider::ResolvedFile>, BoxError> {
        Ok(vec![])
    }

    fn plan(
        &self,
        inputs: &ContainerInputs,
        prior_state: Option<&ContainerState>,
    ) -> Result<PlanResult, BoxError> {
        let desc = format!("docker run {}", inputs.image);

        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description: desc,
            });
        };

        let hash = config_hash(inputs);
        if hash != prior.config_hash {
            return Ok(PlanResult {
                action: PlanAction::Replace,
                description: desc,
            });
        }

        if !container_running(&prior.name) {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description: desc,
            });
        }

        Ok(PlanResult {
            action: PlanAction::None,
            description: desc,
        })
    }

    fn apply(
        &self,
        inputs: &ContainerInputs,
        _prior_state: Option<&ContainerState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<ContainerState, ContainerOutputs>, BoxError> {
        // Remove any existing container with this name (running, stopped, or
        // leftover from a previous run whose state was lost). Ignore errors
        // since the container may not exist.
        let _ = remove_container(&inputs.name);

        let mut cmd = Command::new("docker");
        cmd.arg("run").arg("-d").arg("--name").arg(&inputs.name);

        for port in &inputs.ports {
            cmd.arg("-p").arg(port);
        }
        for vol in &inputs.volumes {
            cmd.arg("-v").arg(vol);
        }
        let mut env_keys: Vec<&String> = inputs.environment.keys().collect();
        env_keys.sort();
        for k in env_keys {
            cmd.arg("-e").arg(format!("{}={}", k, inputs.environment[k]));
        }
        if let Some(ep) = &inputs.entrypoint {
            cmd.arg("--entrypoint").arg(ep);
        }
        cmd.arg("--restart").arg(&inputs.restart);
        if let Some(net) = &inputs.network {
            cmd.arg("--network").arg(net);
        }
        if let Some(wd) = &inputs.working_dir {
            cmd.arg("-w").arg(wd);
        }

        if let Some(hc) = &inputs.healthcheck {
            cmd.arg("--health-cmd").arg(hc.test());
            cmd.arg("--health-interval").arg(hc.interval());
            cmd.arg("--health-timeout").arg(hc.timeout());
            cmd.arg("--health-retries").arg(hc.retries().to_string());
            if let Some(sp) = hc.start_period() {
                cmd.arg("--health-start-period").arg(sp);
            }
        }

        cmd.arg(&inputs.image);

        if let Some(command) = &inputs.command {
            cmd.args(command.split_whitespace());
        }

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| format!("failed to run docker run: {e}"))?;

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

        let status = child.wait().map_err(|e| format!("docker run failed: {e}"))?;
        if !status.success() {
            return Err(format!("docker run exited with {status}").into());
        }

        // Wait for healthcheck to pass if configured; clean up on failure
        if let Some(hc) = &inputs.healthcheck
            && let Err(e) = wait_healthy(&inputs.name, hc, writer)
        {
            let _ = remove_container(&inputs.name);
            return Err(e);
        }

        // Get container ID
        let id_output = Command::new("docker")
            .args(["inspect", "--format", "{{.Id}}", &inputs.name])
            .output()
            .map_err(|e| format!("docker inspect failed: {e}"))?;

        let container_id = String::from_utf8_lossy(&id_output.stdout).trim().to_owned();

        Ok(ApplyResult {
            state: Some(ContainerState {
                name: inputs.name.clone(),
                container_id: container_id.clone(),
                config_hash: config_hash(inputs),
            }),
            outputs: ContainerOutputs {
                container_id,
                name: inputs.name.clone(),
            },
        })
    }

    fn destroy(&self, prior_state: &ContainerState, writer: &BlockWriter) -> Result<(), BoxError> {
        use crate::output::Event;
        writer.event(Event::Starting, &format!("docker rm -f {}", prior_state.name));
        remove_container(&prior_state.name)
    }

    fn refresh(
        &self,
        prior_state: &ContainerState,
    ) -> Result<ApplyResult<ContainerState, ContainerOutputs>, BoxError> {
        if !container_running(&prior_state.name) {
            return Err(format!("container {} is not running", prior_state.name).into());
        }

        let id_output = Command::new("docker")
            .args(["inspect", "--format", "{{.Id}}", &prior_state.name])
            .output()
            .map_err(|e| format!("docker inspect failed: {e}"))?;

        let container_id = String::from_utf8_lossy(&id_output.stdout).trim().to_owned();

        Ok(ApplyResult {
            state: Some(ContainerState {
                name: prior_state.name.clone(),
                container_id: container_id.clone(),
                config_hash: prior_state.config_hash.clone(),
            }),
            outputs: ContainerOutputs {
                container_id,
                name: prior_state.name.clone(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::PlanAction;

    fn test_inputs() -> ContainerInputs {
        ContainerInputs {
            image: "nginx:latest".into(),
            name: "test-nginx".into(),
            ports: vec!["8080:80".into()],
            volumes: vec![],
            environment: HashMap::new(),
            command: None,
            entrypoint: None,
            restart: "no".into(),
            network: None,
            working_dir: None,
            healthcheck: None,
        }
    }

    #[test]
    fn plan_create_when_no_state() {
        let inputs = test_inputs();
        let result = Resource::plan(&ContainerResource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert!(result.description.contains("nginx:latest"));
    }

    #[test]
    fn plan_replace_when_config_changed() {
        let inputs = test_inputs();
        let prior = ContainerState {
            name: "test-nginx".into(),
            container_id: "abc123".into(),
            config_hash: "stale-hash".into(),
        };
        let result = Resource::plan(&ContainerResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Replace);
    }

    #[test]
    fn plan_create_when_container_missing() {
        let inputs = test_inputs();
        let prior = ContainerState {
            name: "nonexistent-container-bit-test".into(),
            container_id: "abc123".into(),
            config_hash: config_hash(&inputs),
        };
        let result = Resource::plan(&ContainerResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn config_hash_deterministic() {
        let inputs = test_inputs();
        assert_eq!(config_hash(&inputs), config_hash(&inputs));
    }

    #[test]
    fn config_hash_changes_with_image() {
        let mut a = test_inputs();
        let mut b = test_inputs();
        a.image = "nginx:1".into();
        b.image = "nginx:2".into();
        assert_ne!(config_hash(&a), config_hash(&b));
    }

    #[test]
    fn config_hash_changes_with_env() {
        let mut a = test_inputs();
        let b = test_inputs();
        a.environment.insert("FOO".into(), "bar".into());
        assert_ne!(config_hash(&a), config_hash(&b));
    }

    #[test]
    fn config_hash_changes_with_healthcheck() {
        let mut a = test_inputs();
        let b = test_inputs();
        a.healthcheck = Some(Healthcheck::Command("curl localhost".into()));
        assert_ne!(config_hash(&a), config_hash(&b));
    }

    #[test]
    fn parse_duration_values() {
        assert_eq!(parse_duration("10s"), Duration::from_secs(10));
        assert_eq!(parse_duration("2m"), Duration::from_secs(120));
        assert_eq!(parse_duration("500ms"), Duration::from_millis(500));
        assert_eq!(parse_duration("30"), Duration::from_secs(30));
    }

    #[test]
    fn healthcheck_command_defaults() {
        let hc = Healthcheck::Command("curl localhost".into());
        assert_eq!(hc.test(), "curl localhost");
        assert_eq!(hc.interval(), "5s");
        assert_eq!(hc.timeout(), "5s");
        assert_eq!(hc.retries(), 3);
        assert!(hc.start_period().is_none());
    }

    #[test]
    fn config_hash_env_order_independent() {
        let mut a = test_inputs();
        let mut b = test_inputs();
        a.environment.insert("A".into(), "1".into());
        a.environment.insert("B".into(), "2".into());
        b.environment.insert("B".into(), "2".into());
        b.environment.insert("A".into(), "1".into());
        assert_eq!(config_hash(&a), config_hash(&b));
    }
}
