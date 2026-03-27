use std::collections::HashMap;
use std::io::BufReader;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::output::BlockWriter;
use crate::provider::{
    ApplyResult, BoxError, PlanAction, PlanResult, ResolveResult, ResolvedInput, ResolvedPath,
    Resource, ResourceKind,
};

#[derive(Debug, Deserialize)]
pub struct ImageInputs {
    pub tag: String,
    #[serde(default = "default_context")]
    pub context: String,
    #[serde(default = "default_dockerfile")]
    pub dockerfile: String,
    #[serde(default)]
    pub build_args: HashMap<String, String>,
}

fn default_context() -> String {
    ".".into()
}

fn default_dockerfile() -> String {
    "Dockerfile".into()
}

#[derive(Debug, Serialize)]
pub struct ImageOutputs {
    #[serde(rename = "ref")]
    pub image_ref: String,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageState {
    pub tag: String,
    pub digest: String,
    pub dockerfile_hash: String,
}

pub struct ImageResource;

impl Resource for ImageResource {
    type State = ImageState;
    type Inputs = ImageInputs;
    type Outputs = ImageOutputs;

    fn name(&self) -> &str {
        "image"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn resolve(&self, inputs: &ImageInputs) -> Result<ResolveResult, BoxError> {
        let mut paths = Vec::new();

        let dockerfile = std::path::Path::new(&inputs.context).join(&inputs.dockerfile);
        if dockerfile.is_file() {
            let contents = std::fs::read(&dockerfile)?;
            let hash = format!("sha256:{:x}", sha2::Sha256::digest(&contents));
            paths.push(ResolvedPath {
                path: dockerfile.to_string_lossy().into_owned(),
                content_hash: hash,
            });
        }

        Ok(ResolveResult {
            inputs: vec![ResolvedInput {
                key: "dockerfile".into(),
                paths,
            }],
            watches: vec![inputs.dockerfile.clone()],
            platform: vec![],
        })
    }

    fn plan(
        &self,
        inputs: &ImageInputs,
        prior_state: Option<&ImageState>,
    ) -> Result<PlanResult, BoxError> {
        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description: format!("docker build -t {}", inputs.tag),
            });
        };

        if prior.tag != inputs.tag {
            return Ok(PlanResult {
                action: PlanAction::Update,
                description: format!("docker build -t {}", inputs.tag),
            });
        }

        let dockerfile = std::path::Path::new(&inputs.context).join(&inputs.dockerfile);
        if dockerfile.is_file() {
            let contents = std::fs::read(&dockerfile)?;
            let hash = format!("sha256:{:x}", sha2::Sha256::digest(&contents));
            if hash != prior.dockerfile_hash {
                return Ok(PlanResult {
                    action: PlanAction::Update,
                    description: format!("docker build -t {}", inputs.tag),
                });
            }
        }

        Ok(PlanResult {
            action: PlanAction::None,
            description: "no changes".into(),
        })
    }

    fn apply(
        &self,
        inputs: &ImageInputs,
        _prior_state: Option<&ImageState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<ImageState, ImageOutputs>, BoxError> {
        let mut cmd = Command::new("docker");
        cmd.arg("build")
            .arg("-t")
            .arg(&inputs.tag)
            .arg("-f")
            .arg(&inputs.dockerfile)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        for (key, val) in &inputs.build_args {
            cmd.arg("--build-arg").arg(format!("{key}={val}"));
        }

        cmd.arg(&inputs.context);

        let mut child = cmd.spawn().map_err(|e| format!("failed to run docker build: {e}"))?;

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

        let status = child.wait().map_err(|e| format!("docker build failed: {e}"))?;
        if !status.success() {
            return Err(format!("docker build exited with {status}").into());
        }

        // Get the digest of the built image
        let digest_output = Command::new("docker")
            .args(["inspect", "--format", "{{.Id}}", &inputs.tag])
            .output()
            .map_err(|e| format!("docker inspect failed: {e}"))?;

        let digest = String::from_utf8_lossy(&digest_output.stdout).trim().to_owned();

        let dockerfile = std::path::Path::new(&inputs.context).join(&inputs.dockerfile);
        let dockerfile_hash = if dockerfile.is_file() {
            let contents = std::fs::read(&dockerfile)?;
            format!("sha256:{:x}", sha2::Sha256::digest(&contents))
        } else {
            String::new()
        };

        Ok(ApplyResult {
            outputs: ImageOutputs {
                image_ref: inputs.tag.clone(),
                digest: digest.clone(),
            },
            state: Some(ImageState {
                tag: inputs.tag.clone(),
                digest,
                dockerfile_hash,
            }),
        })
    }

    fn destroy(&self, prior_state: &ImageState, writer: &BlockWriter) -> Result<(), BoxError> {
        writer.line(&format!("docker rmi {}", prior_state.tag));
        let output = Command::new("docker")
            .args(["rmi", &prior_state.tag])
            .output()
            .map_err(|e| format!("docker rmi failed: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            writer.stderr_line(stderr.trim());
        }
        Ok(())
    }

    fn refresh(
        &self,
        prior_state: &ImageState,
    ) -> Result<ApplyResult<ImageState, ImageOutputs>, BoxError> {
        let output = Command::new("docker")
            .args(["inspect", "--format", "{{.Id}}", &prior_state.tag])
            .output()
            .map_err(|e| format!("docker inspect failed: {e}"))?;

        if !output.status.success() {
            return Err(format!("image {} not found", prior_state.tag).into());
        }

        let digest = String::from_utf8_lossy(&output.stdout).trim().to_owned();

        Ok(ApplyResult {
            outputs: ImageOutputs {
                image_ref: prior_state.tag.clone(),
                digest: digest.clone(),
            },
            state: Some(ImageState {
                digest,
                ..prior_state.clone()
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_create_when_no_state() {
        let inputs = ImageInputs {
            tag: "myapp:latest".into(),
            context: ".".into(),
            dockerfile: "Dockerfile".into(),
            build_args: HashMap::new(),
        };
        let result = Resource::plan(&ImageResource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert!(result.description.contains("myapp:latest"));
    }

    #[test]
    fn plan_none_when_unchanged() {
        let inputs = ImageInputs {
            tag: "myapp:latest".into(),
            context: ".".into(),
            dockerfile: "nonexistent".into(),
            build_args: HashMap::new(),
        };
        let prior = ImageState {
            tag: "myapp:latest".into(),
            digest: "sha256:abc".into(),
            dockerfile_hash: String::new(),
        };
        let result = Resource::plan(&ImageResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::None);
    }

    #[test]
    fn plan_update_when_tag_changed() {
        let inputs = ImageInputs {
            tag: "myapp:v2".into(),
            context: ".".into(),
            dockerfile: "Dockerfile".into(),
            build_args: HashMap::new(),
        };
        let prior = ImageState {
            tag: "myapp:v1".into(),
            digest: "sha256:abc".into(),
            dockerfile_hash: String::new(),
        };
        let result = Resource::plan(&ImageResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn plan_update_when_dockerfile_changed() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        std::fs::write(&dockerfile, "FROM alpine\nRUN echo new").unwrap();

        let inputs = ImageInputs {
            tag: "myapp:latest".into(),
            context: dir.path().to_string_lossy().into_owned(),
            dockerfile: "Dockerfile".into(),
            build_args: HashMap::new(),
        };
        let prior = ImageState {
            tag: "myapp:latest".into(),
            digest: "sha256:abc".into(),
            dockerfile_hash: "sha256:old".into(),
        };
        let result = Resource::plan(&ImageResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }
}
