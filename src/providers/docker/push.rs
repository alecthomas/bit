use crate::process::CommandExt;
use std::io::BufReader;
use std::process::Stdio;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::TargetWriter;
use crate::provider::{
    Ambient, ApplyResult, BoxError, CachePolicy, Changes, PlanAction, PlanResult, Resource, ResourceKind, Target,
};
use crate::sha256::SHA256;

/// Push a Docker image to a registry. Pushes again only when the image
/// changes, not when the registry copy was deleted or overwritten. Pushing a
/// new image removes the local digest reference of the one pushed before.
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct PushInputs {
    /// Source image reference (e.g. from a docker.image target's ref)
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
    /// The digest references `docker push` gave the image, `repo@sha256:…`,
    /// which name it locally until removed.
    #[serde(default)]
    pub digests: Vec<String>,
}

/// The repository `tag` names: everything before its tag, if it has one.
/// A registry's port also follows a `:`, but comes before the last `/`.
fn repository(tag: &str) -> &str {
    match tag.rsplit_once(':') {
        Some((repository, tag)) if !tag.contains('/') => repository,
        _ => tag,
    }
}

/// The digest references image `id` has in the repository `tag` was pushed
/// to.
fn pushed_digests(env: &super::Env, id: &str, tag: &str) -> Result<Vec<String>, BoxError> {
    let output = super::docker(env)
        .args([
            "image",
            "inspect",
            "--format",
            "{{range .RepoDigests}}{{println .}}{{end}}",
            id,
        ])
        .logged_output()
        .map_err(|e| format!("docker image inspect failed: {e}"))?;
    if !output.status.success() {
        return Err(format!("image {id} not found").into());
    }
    let prefix = format!("{}@", repository(tag));
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with(&prefix))
        .map(str::to_owned)
        .collect())
}

/// Remove the digest references in `digests`, which leave the image itself
/// to any other name it has.
fn remove_digests(env: &super::Env, digests: &[String], writer: &TargetWriter) -> Result<(), BoxError> {
    use crate::output::Event;
    for digest in digests {
        writer.event(Event::Starting, &format!("docker rmi {digest}"));
        let output = super::docker(env)
            .args(["rmi", digest])
            .logged_output()
            .map_err(|e| format!("docker rmi failed: {e}"))?;
        super::check_remove_output("docker rmi", output)?;
    }
    Ok(())
}

fn inspect_id(env: &super::Env, image: &str) -> Result<String, BoxError> {
    let output = super::docker(env)
        .args(["inspect", "--format", "{{.Id}}", image])
        .logged_output()
        .map_err(|e| format!("docker inspect failed: {e}"))?;
    if !output.status.success() {
        return Err(format!("image {image} not found").into());
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(raw.strip_prefix("sha256:").unwrap_or(&raw).to_owned())
}

pub struct PushResource {
    #[allow(dead_code)]
    tracker: Arc<FileTracker>,
}

impl PushResource {
    pub(super) fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }
}

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

    fn ambient(&self, _inputs: &Target<PushInputs>) -> Vec<Ambient> {
        vec![crate::providers::version("docker", &["--version"], None)]
    }

    fn resolve(&self, _inputs: &Target<PushInputs>) -> Result<std::collections::BTreeMap<String, SHA256>, BoxError> {
        Ok(std::collections::BTreeMap::new())
    }

    fn plan(&self, inputs: &Target<PushInputs>, prior_state: Option<&PushState>) -> Result<PlanResult, BoxError> {
        let desc = format!(
            "docker tag {} {} && docker push {}",
            inputs.image, inputs.tag, inputs.tag
        );

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

        let current_id = inspect_id(&inputs.env, &inputs.image).unwrap_or_default();
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
        inputs: &Target<PushInputs>,
        prior_state: Option<&PushState>,
        _changes: &Changes,
        writer: &TargetWriter,
    ) -> Result<ApplyResult<PushState, PushOutputs>, BoxError> {
        let tag_status = super::docker(&inputs.env)
            .args(["tag", &inputs.image, &inputs.tag])
            .logged_output()
            .map_err(|e| format!("docker tag failed: {e}"))?;
        if !tag_status.status.success() {
            return Err(format!(
                "docker tag failed: {}",
                String::from_utf8_lossy(&tag_status.stderr).trim()
            )
            .into());
        }

        let mut cmd = super::docker(&inputs.env);
        cmd.args(["push", &inputs.tag])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.logged_spawn().map_err(|e| format!("docker push failed: {e}"))?;
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

        let image_id = inspect_id(&inputs.env, &inputs.tag)?;
        let digests = pushed_digests(&inputs.env, &image_id, &inputs.tag)?;
        // The image pushed before keeps its digest reference, and with it
        // stays on disk, after its other names are gone.
        if let Some(prior) = prior_state.filter(|prior| prior.image_id != image_id) {
            let replaced: Vec<String> = prior
                .digests
                .iter()
                .filter(|digest| !digests.contains(digest))
                .cloned()
                .collect();
            remove_digests(&inputs.env, &replaced, writer)?;
        }

        Ok(ApplyResult {
            state: Some(PushState {
                image: inputs.image.clone(),
                tag: inputs.tag.clone(),
                image_id,
                digests,
            }),
            outputs: PushOutputs {
                image_ref: inputs.tag.clone(),
            },
        })
    }

    fn destroy(&self, prior_state: &Target<PushState>, writer: &TargetWriter) -> Result<(), BoxError> {
        use crate::output::Event;
        writer.event(Event::Starting, &format!("docker rmi {}", prior_state.tag));
        let output = super::docker(&prior_state.env)
            .args(["rmi", &prior_state.tag])
            .logged_output()
            .map_err(|e| format!("docker rmi failed: {e}"))?;
        super::check_remove_output("docker rmi", output)?;
        remove_digests(&prior_state.env, &prior_state.digests, writer)
    }

    /// A push mutates a remote registry. Another worktree's receipt says
    /// nothing about whether this tag is still there.
    fn cache_policy(&self, _inputs: &Target<PushInputs>) -> CachePolicy {
        CachePolicy::Local
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_create_when_no_state() {
        let inputs = Target::new(
            None,
            PushInputs {
                image: "myapp:latest".into(),
                tag: "localhost:5000/myapp:abc123".into(),
            },
        );
        let result = Resource::plan(&PushResource::new(Arc::new(FileTracker::default())), &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert_eq!(
            result.description,
            "docker tag myapp:latest localhost:5000/myapp:abc123 && docker push localhost:5000/myapp:abc123"
        );
    }

    #[test]
    fn plan_update_when_tag_changed() {
        let inputs = Target::new(
            None,
            PushInputs {
                image: "myapp:latest".into(),
                tag: "localhost:5000/myapp:def456".into(),
            },
        );
        let prior = PushState {
            image: "myapp:latest".into(),
            tag: "localhost:5000/myapp:abc123".into(),
            image_id: "abc".into(),
            digests: vec![],
        };
        let result = Resource::plan(
            &PushResource::new(Arc::new(FileTracker::default())),
            &inputs,
            Some(&prior),
        )
        .unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn repository_drops_the_tag_but_not_a_registry_port() {
        assert_eq!(repository("localhost:5000/myapp:v1"), "localhost:5000/myapp");
        assert_eq!(repository("localhost:5000/myapp"), "localhost:5000/myapp");
        assert_eq!(repository("myapp:v1"), "myapp");
        assert_eq!(repository("ghcr.io/acme/app"), "ghcr.io/acme/app");
    }
}
