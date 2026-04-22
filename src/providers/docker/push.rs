use std::io::BufReader;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::output::BlockWriter;
use crate::provider::{ApplyResult, BoxError, PlanAction, PlanResult, Resource, ResourceKind};

/// Push a Docker image to a registry
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct PushInputs {
    /// Source image reference (e.g. from a docker.image block's ref)
    pub image: String,
    /// Destination tag including registry (e.g. "localhost:5000/app:abc123")
    pub tag: String,
}

#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct PushOutputs {
    /// Pushed image reference
    #[serde(rename = "ref")]
    pub image_ref: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushState {
    pub image: String,
    pub tag: String,
    pub image_id: String,
}

fn inspect_id(image: &str) -> Result<String, BoxError> {
    let output = Command::new("docker")
        .args(["inspect", "--format", "{{.Id}}", image])
        .output()
        .map_err(|e| format!("docker inspect failed: {e}"))?;
    if !output.status.success() {
        return Err(format!("image {image} not found").into());
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(raw.strip_prefix("sha256:").unwrap_or(&raw).to_owned())
}

pub struct PushResource;

impl Resource for PushResource {
    type State = PushState;
    type Inputs = PushInputs;
    type Outputs = PushOutputs;

    fn name(&self) -> &str {
        "push"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn resolve(&self, _inputs: &PushInputs) -> Result<Vec<crate::provider::ResolvedFile>, BoxError> {
        Ok(vec![])
    }

    fn plan(&self, inputs: &PushInputs, prior_state: Option<&PushState>) -> Result<PlanResult, BoxError> {
        let desc = format!("docker push {}", inputs.tag);

        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description: desc,
                reason: None,
            });
        };

        if prior.tag != inputs.tag {
            return Ok(PlanResult {
                action: PlanAction::Update,
                description: desc,
                reason: Some("tag changed".into()),
            });
        }

        let current_id = inspect_id(&inputs.image).unwrap_or_default();
        if current_id != prior.image_id {
            return Ok(PlanResult {
                action: PlanAction::Update,
                description: desc,
                reason: Some("image changed".into()),
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
        inputs: &PushInputs,
        _prior_state: Option<&PushState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<PushState, PushOutputs>, BoxError> {
        let tag_status = Command::new("docker")
            .args(["tag", &inputs.image, &inputs.tag])
            .output()
            .map_err(|e| format!("docker tag failed: {e}"))?;
        if !tag_status.status.success() {
            return Err(format!(
                "docker tag failed: {}",
                String::from_utf8_lossy(&tag_status.stderr).trim()
            )
            .into());
        }

        let mut cmd = Command::new("docker");
        cmd.args(["push", &inputs.tag])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| format!("docker push failed: {e}"))?;
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

        let status = child.wait().map_err(|e| format!("docker push failed: {e}"))?;
        if !status.success() {
            return Err(format!("docker push exited with {status}").into());
        }

        let image_id = inspect_id(&inputs.tag)?;

        Ok(ApplyResult {
            state: Some(PushState {
                image: inputs.image.clone(),
                tag: inputs.tag.clone(),
                image_id,
            }),
            outputs: PushOutputs {
                image_ref: inputs.tag.clone(),
            },
        })
    }

    fn destroy(&self, prior_state: &PushState, writer: &BlockWriter) -> Result<(), BoxError> {
        use crate::output::Event;
        writer.event(Event::Starting, &format!("docker rmi {}", prior_state.tag));
        let _ = Command::new("docker").args(["rmi", &prior_state.tag]).output();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_create_when_no_state() {
        let inputs = PushInputs {
            image: "myapp:latest".into(),
            tag: "localhost:5000/myapp:abc123".into(),
        };
        let result = Resource::plan(&PushResource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn plan_update_when_tag_changed() {
        let inputs = PushInputs {
            image: "myapp:latest".into(),
            tag: "localhost:5000/myapp:def456".into(),
        };
        let prior = PushState {
            image: "myapp:latest".into(),
            tag: "localhost:5000/myapp:abc123".into(),
            image_id: "abc".into(),
        };
        let result = Resource::plan(&PushResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }
}
