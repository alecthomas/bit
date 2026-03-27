use std::collections::HashMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use parse_dockerfile::Instruction;
use serde::{Deserialize, Serialize};

use crate::output::BlockWriter;
use crate::provider::{ApplyResult, BoxError, PlanAction, PlanResult, Resource, ResourceKind};

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
    pub image_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageState {
    pub tag: String,
}

/// Parse a Dockerfile and return local source paths from COPY/ADD instructions.
/// Skips multi-stage `COPY --from=...` and ADD with URLs.
fn parse_dockerfile_sources(dockerfile: &Path, context: &Path) -> Vec<PathBuf> {
    let Ok(contents) = std::fs::read_to_string(dockerfile) else {
        return vec![];
    };
    let Ok(parsed) = parse_dockerfile::parse(&contents) else {
        return vec![];
    };

    let mut paths = Vec::new();
    for instruction in &parsed.instructions {
        match instruction {
            Instruction::Copy(copy) => {
                // Skip multi-stage copies (COPY --from=builder ...)
                let has_from = copy.options.iter().any(|f| &*f.name.value == "from");
                if has_from {
                    continue;
                }
                for src in &copy.src {
                    if let parse_dockerfile::Source::Path(p) = src {
                        let src_path = &*p.value;
                        paths.push(context.join(src_path));
                    }
                }
            }
            Instruction::Add(add) => {
                for src in &add.src {
                    if let parse_dockerfile::Source::Path(p) = src {
                        let src_path = &*p.value;
                        // Skip URLs
                        if src_path.starts_with("http://") || src_path.starts_with("https://") {
                            continue;
                        }
                        paths.push(context.join(src_path));
                    }
                }
            }
            _ => {}
        }
    }
    paths
}

/// Expand a path (which may be a glob or directory) into individual files,
/// returning (path, content_hash) pairs.
/// Expand a path (file, directory, or glob) into individual files.
fn expand_path(path: &Path) -> Vec<PathBuf> {
    let pattern = path.to_string_lossy();

    if path.is_dir() {
        let glob_pattern = format!("{}/**/*", pattern);
        if let Ok(entries) = glob::glob(&glob_pattern) {
            return entries.flatten().filter(|e| e.is_file()).collect();
        }
        return vec![];
    }

    if let Ok(entries) = glob::glob(&pattern) {
        return entries.flatten().filter(|e| e.is_file()).collect();
    }
    vec![]
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

    fn resolve(&self, inputs: &ImageInputs) -> Result<crate::provider::ResolvedFiles, BoxError> {
        let context = Path::new(&inputs.context);
        let dockerfile = context.join(&inputs.dockerfile);

        let mut input_files = Vec::new();

        // Include the Dockerfile itself
        if dockerfile.is_file() {
            input_files.push(dockerfile.clone());
        }

        // Parse Dockerfile to discover COPY/ADD source paths
        for src in &parse_dockerfile_sources(&dockerfile, context) {
            input_files.extend(expand_path(src));
        }

        // Docker images don't produce local output files
        Ok(crate::provider::ResolvedFiles {
            inputs: input_files,
            outputs: vec![],
        })
    }

    fn plan(&self, inputs: &ImageInputs, prior_state: Option<&ImageState>) -> Result<PlanResult, BoxError> {
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

        let image_id = String::from_utf8_lossy(&digest_output.stdout).trim().to_owned();

        Ok(ApplyResult {
            outputs: ImageOutputs {
                image_ref: inputs.tag.clone(),
                image_id,
            },
            state: Some(ImageState {
                tag: inputs.tag.clone(),
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

    fn refresh(&self, prior_state: &ImageState) -> Result<ApplyResult<ImageState, ImageOutputs>, BoxError> {
        let output = Command::new("docker")
            .args(["inspect", "--format", "{{.Id}}", &prior_state.tag])
            .output()
            .map_err(|e| format!("docker inspect failed: {e}"))?;

        if !output.status.success() {
            return Err(format!("image {} not found", prior_state.tag).into());
        }

        let image_id = String::from_utf8_lossy(&output.stdout).trim().to_owned();

        Ok(ApplyResult {
            outputs: ImageOutputs {
                image_ref: prior_state.tag.clone(),
                image_id,
            },
            state: Some(prior_state.clone()),
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
            dockerfile: "Dockerfile".into(),
            build_args: HashMap::new(),
        };
        let prior = ImageState {
            tag: "myapp:latest".into(),
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
        let prior = ImageState { tag: "myapp:v1".into() };
        let result = Resource::plan(&ImageResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn parse_dockerfile_extracts_copy_sources() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        std::fs::write(
            &dockerfile,
            "FROM alpine\nCOPY src/ /app/src/\nCOPY config.toml /app/\n",
        )
        .unwrap();

        let sources = parse_dockerfile_sources(&dockerfile, dir.path());
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0], dir.path().join("src/"));
        assert_eq!(sources[1], dir.path().join("config.toml"));
    }

    #[test]
    fn parse_dockerfile_skips_multistage_copy() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        std::fs::write(
            &dockerfile,
            concat!(
                "FROM rust AS builder\n",
                "COPY src/ /src/\n",
                "FROM debian\n",
                "COPY --from=builder /src/target/app /usr/bin/app\n",
            ),
        )
        .unwrap();

        let sources = parse_dockerfile_sources(&dockerfile, dir.path());
        // Only the first COPY, not the --from=builder one
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0], dir.path().join("src/"));
    }

    #[test]
    fn parse_dockerfile_skips_add_urls() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        std::fs::write(
            &dockerfile,
            "FROM alpine\nADD https://example.com/file.tar.gz /app/\nADD local.txt /app/\n",
        )
        .unwrap();

        let sources = parse_dockerfile_sources(&dockerfile, dir.path());
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0], dir.path().join("local.txt"));
    }

    #[test]
    fn resolve_includes_copy_sources() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        let src_file = dir.path().join("app.txt");
        std::fs::write(&dockerfile, "FROM alpine\nCOPY app.txt /app/\n").unwrap();
        std::fs::write(&src_file, "hello").unwrap();

        let inputs = ImageInputs {
            tag: "myapp:latest".into(),
            context: dir.path().to_string_lossy().into_owned(),
            dockerfile: "Dockerfile".into(),
            build_args: HashMap::new(),
        };
        let resolved = Resource::resolve(&ImageResource, &inputs).unwrap();
        assert_eq!(resolved.inputs.len(), 2);
        assert!(resolved.inputs.contains(&dockerfile));
        assert!(resolved.inputs.contains(&src_file));
        assert!(resolved.outputs.is_empty());
    }
}
