use std::collections::HashMap;
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{
    ApplyResult, Block, BoxError, CachePolicy, Changes, PlanAction, PlanResult, Resource, ResourceKind,
};
use crate::sha256::SHA256;
use crate::value::Duration;

/// Healthcheck config — either a bare command string or a full object
/// with interval/timeout/retries/start_period.
#[derive(Debug, Clone, Deserialize, bit_derive::Schema)]
#[serde(untagged)]
pub enum Healthcheck {
    /// Shell command to run
    Command(String),
    Full {
        /// Command to run
        test: String,
        /// How often to run the check (default: 1s)
        #[serde(default = "default_interval")]
        interval: Duration,
        /// How long a check can take before it counts as a failure (default: 5s)
        #[serde(default = "default_timeout")]
        timeout: Duration,
        /// Number of consecutive failures required to mark unhealthy (default: 3)
        #[serde(default = "default_retries")]
        retries: u32,
        /// Grace period before failures count
        #[serde(default)]
        start_period: Option<Duration>,
    },
}

impl Healthcheck {
    fn test(&self) -> &str {
        match self {
            Healthcheck::Command(cmd) => cmd,
            Healthcheck::Full { test, .. } => test,
        }
    }

    fn interval(&self) -> Duration {
        match self {
            Healthcheck::Command(_) => default_interval(),
            Healthcheck::Full { interval, .. } => *interval,
        }
    }

    fn timeout(&self) -> Duration {
        match self {
            Healthcheck::Command(_) => default_timeout(),
            Healthcheck::Full { timeout, .. } => *timeout,
        }
    }

    fn retries(&self) -> u32 {
        match self {
            Healthcheck::Command(_) => default_retries(),
            Healthcheck::Full { retries, .. } => *retries,
        }
    }

    fn start_period(&self) -> Option<Duration> {
        match self {
            Healthcheck::Command(_) => None,
            Healthcheck::Full { start_period, .. } => *start_period,
        }
    }

    /// Total deadline: start_period + (interval * retries) + timeout.
    fn deadline(&self) -> Duration {
        self.start_period().unwrap_or_default() + self.interval() * self.retries() + self.timeout()
    }

    fn poll_interval(&self) -> Duration {
        self.interval()
    }
}

fn default_interval() -> Duration {
    Duration::from_secs(1)
}

fn default_timeout() -> Duration {
    Duration::from_secs(5)
}

fn default_retries() -> u32 {
    3
}

/// Run a Docker container (tracks state like Terraform)
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct ContainerInputs {
    /// Docker image reference
    pub image: String,
    /// Container name
    pub name: String,
    /// Port mappings (e.g. "8080:80")
    #[serde(default)]
    pub ports: Vec<String>,
    /// Volume mounts
    #[serde(default)]
    pub volumes: Vec<String>,
    /// Environment variables
    #[serde(default)]
    pub environment: HashMap<String, String>,
    /// Override CMD
    #[serde(default)]
    pub command: Option<String>,
    /// Override ENTRYPOINT
    #[serde(default)]
    pub entrypoint: Option<String>,
    /// Restart policy
    #[serde(default = "default_restart")]
    pub restart: String,
    /// Docker network
    #[serde(default)]
    pub network: Option<String>,
    /// Working directory
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Health check command or config
    #[serde(default)]
    pub healthcheck: Option<Healthcheck>,
    /// Extra /etc/hosts entries (hostname → address). On Linux,
    /// `host.docker.internal: host-gateway` is auto-added if not present.
    #[serde(default)]
    pub extra_hosts: HashMap<String, String>,
}

/// Merge the user's `extra_hosts` with platform defaults. On Linux we
/// inject `host.docker.internal: host-gateway` when absent so containers
/// can reach host services consistently with Docker Desktop on
/// Mac/Windows. The result is sorted by hostname for deterministic CLI
/// arg order (important for plan stability / tests).
fn resolve_extra_hosts(user: &HashMap<String, String>, is_linux: bool) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = user.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    if is_linux && !user.contains_key("host.docker.internal") {
        out.push(("host.docker.internal".into(), "host-gateway".into()));
    }
    out.sort();
    out
}

fn default_restart() -> String {
    "no".into()
}

#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct ContainerOutputs {
    /// Docker container ID
    pub container_id: String,
    /// Container name
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerState {
    pub name: String,
    pub container_id: String,
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
    super::check_remove_output("docker rm", output)
}

/// Poll `docker inspect` until the container's health status is "healthy"
/// or the deadline is exceeded.
fn wait_healthy(name: &str, hc: &Healthcheck, writer: &BlockWriter) -> Result<(), BoxError> {
    let deadline = hc.deadline();
    let interval = hc.poll_interval();
    let start = Instant::now();

    writer.line("waiting for healthcheck...");

    loop {
        std::thread::sleep(interval.as_std());

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
                if start.elapsed() > deadline.as_std() {
                    return Err(format!("container healthcheck timed out after {}s", deadline.as_secs()).into());
                }
            }
        }
    }
}

/// Whether [`run_args`] includes environment values.
#[derive(Clone, Copy, PartialEq)]
enum EnvValues {
    Shown,
    /// Values may hold secrets, so a displayed command replaces them.
    Masked,
}

/// The arguments to `docker` that start the block's container.
fn run_args(inputs: &ContainerInputs, env_values: EnvValues) -> Vec<String> {
    let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--name".into(), inputs.name.clone()];
    for port in &inputs.ports {
        args.extend(["-p".into(), port.clone()]);
    }
    for vol in &inputs.volumes {
        args.extend(["-v".into(), vol.clone()]);
    }
    let mut env_keys: Vec<&String> = inputs.environment.keys().collect();
    env_keys.sort();
    for k in env_keys {
        let value = match env_values {
            EnvValues::Shown => inputs.environment[k].as_str(),
            EnvValues::Masked => "***",
        };
        args.extend(["-e".into(), format!("{k}={value}")]);
    }
    if let Some(ep) = &inputs.entrypoint {
        args.extend(["--entrypoint".into(), ep.clone()]);
    }
    args.extend(["--restart".into(), inputs.restart.clone()]);
    if let Some(net) = &inputs.network {
        args.extend(["--network".into(), net.clone()]);
    }
    for (host, addr) in resolve_extra_hosts(&inputs.extra_hosts, cfg!(target_os = "linux")) {
        args.extend(["--add-host".into(), format!("{host}:{addr}")]);
    }
    if let Some(wd) = &inputs.working_dir {
        args.extend(["-w".into(), wd.clone()]);
    }
    if let Some(hc) = &inputs.healthcheck {
        args.extend(["--health-cmd".into(), hc.test().to_owned()]);
        args.extend(["--health-interval".into(), hc.interval().to_string()]);
        args.extend(["--health-timeout".into(), hc.timeout().to_string()]);
        args.extend(["--health-retries".into(), hc.retries().to_string()]);
        if let Some(sp) = hc.start_period() {
            args.extend(["--health-start-period".into(), sp.to_string()]);
        }
    }
    args.push(inputs.image.clone());
    if let Some(command) = &inputs.command {
        args.extend(command.split_whitespace().map(str::to_owned));
    }
    args
}

pub struct ContainerResource {
    #[allow(dead_code)]
    tracker: Arc<FileTracker>,
}

impl ContainerResource {
    pub(super) fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

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

    fn resolve(
        &self,
        _inputs: &Block<ContainerInputs>,
    ) -> Result<std::collections::BTreeMap<String, SHA256>, BoxError> {
        Ok(std::collections::BTreeMap::new())
    }

    /// None: the container is daemon state that the docker CLI only
    /// requests, so a CLI upgrade must not recreate a running container.
    fn ambient(&self, _inputs: &Block<ContainerInputs>) -> Vec<crate::provider::Ambient> {
        vec![]
    }

    fn plan(
        &self,
        inputs: &Block<ContainerInputs>,
        prior_state: Option<&ContainerState>,
    ) -> Result<PlanResult, BoxError> {
        let desc = crate::providers::display_command("docker", &run_args(inputs, EnvValues::Masked));

        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description: desc,
                reason: None,
            });
        };

        // Re-create if the container drifted away (was stopped/removed out of
        // band). When the block's inputs change, the engine sees a hash delta
        // and upgrades None → Update.
        if !container_running(&prior.name) {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description: desc,
                reason: Some("container not running".into()),
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
        inputs: &Block<ContainerInputs>,
        _prior_state: Option<&ContainerState>,
        _changes: &Changes,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<ContainerState, ContainerOutputs>, BoxError> {
        // Remove any existing container with this name (running, stopped, or
        // leftover from a previous run whose state was lost). Ignore errors
        // since the container may not exist.
        let _ = remove_container(&inputs.name);

        let mut cmd = Command::new("docker");
        cmd.args(run_args(inputs, EnvValues::Shown));
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
            }),
            outputs: ContainerOutputs {
                container_id,
                name: inputs.name.clone(),
            },
        })
    }

    fn destroy(&self, prior_state: &Block<ContainerState>, writer: &BlockWriter) -> Result<(), BoxError> {
        use crate::output::Event;
        writer.event(Event::Starting, &format!("docker rm -f {}", prior_state.name));
        remove_container(&prior_state.name)
    }

    /// A container is named, mutable state in the local Docker daemon.
    /// Whether it is running is a fact about the daemon now, not about the
    /// inputs that created it.
    fn cache_policy(&self, _inputs: &Block<ContainerInputs>) -> CachePolicy {
        CachePolicy::Local
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::PlanAction;

    fn test_inputs() -> Block<ContainerInputs> {
        Block::new(
            None,
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
                extra_hosts: HashMap::new(),
            },
        )
    }

    #[test]
    fn plan_create_when_no_state() {
        let inputs = test_inputs();
        let result = Resource::plan(&ContainerResource::new(Arc::new(FileTracker::default())), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert!(result.description.contains("nginx:latest"));
    }

    #[test]
    fn description_shows_run_flags_with_masked_environment() {
        let mut inputs = test_inputs();
        inputs.environment.insert("TOKEN".into(), "s3cr3t".into());
        let result = Resource::plan(&ContainerResource::new(Arc::new(FileTracker::default())), &inputs, None).unwrap();
        assert!(
            result
                .description
                .starts_with("docker run -d --name test-nginx -p 8080:80 -e TOKEN=*** --restart no"),
            "{}",
            result.description
        );
        assert!(!result.description.contains("s3cr3t"));
        assert!(run_args(&inputs, EnvValues::Shown).contains(&"TOKEN=s3cr3t".to_owned()));
    }

    #[test]
    fn plan_create_when_container_missing() {
        let inputs = test_inputs();
        let prior = ContainerState {
            name: "nonexistent-container-bit-test".into(),
            container_id: "abc123".into(),
        };
        let result = Resource::plan(
            &ContainerResource::new(Arc::new(FileTracker::default())),
            &inputs,
            Some(&prior),
        )
        .unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn resolve_extra_hosts_passes_through_user_entries() {
        let mut user = HashMap::new();
        user.insert("legacy.db".to_owned(), "10.0.1.5".to_owned());
        user.insert("search".to_owned(), "10.0.1.6".to_owned());
        let resolved = resolve_extra_hosts(&user, /*is_linux=*/ false);
        assert_eq!(
            resolved,
            vec![
                ("legacy.db".to_owned(), "10.0.1.5".to_owned()),
                ("search".to_owned(), "10.0.1.6".to_owned()),
            ]
        );
    }

    #[test]
    fn resolve_extra_hosts_injects_host_gateway_on_linux() {
        let resolved = resolve_extra_hosts(&HashMap::new(), /*is_linux=*/ true);
        assert_eq!(
            resolved,
            vec![("host.docker.internal".to_owned(), "host-gateway".to_owned())]
        );
    }

    #[test]
    fn resolve_extra_hosts_respects_user_host_internal_override() {
        let mut user = HashMap::new();
        user.insert("host.docker.internal".to_owned(), "1.2.3.4".to_owned());
        let resolved = resolve_extra_hosts(&user, /*is_linux=*/ true);
        // User override wins; no duplicate injected.
        assert_eq!(
            resolved,
            vec![("host.docker.internal".to_owned(), "1.2.3.4".to_owned())]
        );
    }

    #[test]
    fn resolve_extra_hosts_no_inject_on_non_linux() {
        let resolved = resolve_extra_hosts(&HashMap::new(), /*is_linux=*/ false);
        assert!(resolved.is_empty());
    }

    #[test]
    fn healthcheck_command_defaults() {
        let hc = Healthcheck::Command("curl localhost".into());
        assert_eq!(hc.test(), "curl localhost");
        assert_eq!(hc.interval(), Duration::from_secs(1));
        assert_eq!(hc.timeout(), Duration::from_secs(5));
        assert_eq!(hc.retries(), 3);
        assert!(hc.start_period().is_none());
    }
}
