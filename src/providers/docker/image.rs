use std::collections::HashMap;
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::output::BlockWriter;
use crate::provider::{
    ApplyResult, BoxError, FieldSchema, PlanAction, PlanResult, Resource, ResourceKind, ResourceSchema,
};
use crate::value::{Type, Value};

use super::parse;

#[derive(Debug, Deserialize)]
pub struct ImageInputs {
    pub tag: String,
    #[serde(default = "default_context")]
    pub context: String,
    #[serde(default = "default_dockerfile")]
    pub dockerfile: String,
    #[serde(default)]
    pub build_args: HashMap<String, String>,
    #[serde(default, deserialize_with = "string_or_vec")]
    pub platform: Vec<String>,
}

fn default_context() -> String {
    ".".into()
}

fn default_dockerfile() -> String {
    "Dockerfile".into()
}

/// Deserialize a field that can be either a single string or a list of strings.
fn string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct StringOrVec;

    impl<'de> de::Visitor<'de> for StringOrVec {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string or list of strings")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Vec<String>, E> {
            Ok(vec![v.to_owned()])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<String>, A::Error> {
            let mut vec = Vec::new();
            while let Some(s) = seq.next_element()? {
                vec.push(s);
            }
            Ok(vec)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Vec<String>, E> {
            Ok(Vec::new())
        }
    }

    deserializer.deserialize_any(StringOrVec)
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
    pub image_id: String,
    #[serde(default)]
    pub platform: Vec<String>,
}

/// Build the argument list for `docker buildx build`.
fn build_args(inputs: &ImageInputs) -> Vec<String> {
    let mut args = vec![
        "buildx".into(),
        "build".into(),
        "-t".into(),
        inputs.tag.clone(),
        "-f".into(),
        inputs.dockerfile.clone(),
    ];
    if inputs.platform.len() > 1 {
        args.push("--platform".into());
        args.push(inputs.platform.join(","));
        args.push("--push".into());
    } else {
        if inputs.platform.len() == 1 {
            args.push("--platform".into());
            args.push(inputs.platform[0].clone());
        }
        args.push("--load".into());
    }
    for (key, val) in &inputs.build_args {
        args.push("--build-arg".into());
        args.push(format!("{key}={val}"));
    }
    args.push(inputs.context.clone());
    args
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

    fn schema(&self) -> ResourceSchema {
        ResourceSchema {
            description: "Build a Docker image (auto-detects inputs from Dockerfile)".into(),
            kind: ResourceKind::Build,
            inputs: vec![
                FieldSchema {
                    name: "tag".into(),
                    typ: Type::String,
                    required: true,
                    default: None,
                    description: Some("Image tag".into()),
                },
                FieldSchema {
                    name: "context".into(),
                    typ: Type::String,
                    required: false,
                    default: Some(Value::Str(".".into())),
                    description: Some("Build context directory".into()),
                },
                FieldSchema {
                    name: "dockerfile".into(),
                    typ: Type::String,
                    required: false,
                    default: Some(Value::Str("Dockerfile".into())),
                    description: Some("Dockerfile path".into()),
                },
                FieldSchema {
                    name: "build_args".into(),
                    typ: Type::Map(Box::new(Type::String)),
                    required: false,
                    default: None,
                    description: Some("Docker build arguments".into()),
                },
                FieldSchema {
                    name: "platform".into(),
                    typ: Type::Union(vec![Type::String, Type::List(Box::new(Type::String))]),
                    required: false,
                    default: None,
                    description: Some("Target platform(s) (e.g. \"linux/amd64\")".into()),
                },
            ],
            outputs: vec![
                FieldSchema {
                    name: "ref".into(),
                    typ: Type::String,
                    required: true,
                    default: None,
                    description: Some("Image tag/reference".into()),
                },
                FieldSchema {
                    name: "image_id".into(),
                    typ: Type::String,
                    required: true,
                    default: None,
                    description: Some("Docker image ID".into()),
                },
            ],
        }
    }

    fn resolve(&self, inputs: &ImageInputs) -> Result<Vec<crate::provider::ResolvedFile>, BoxError> {
        use crate::provider::ResolvedFile;
        let context = Path::new(&inputs.context);
        let dockerfile = context.join(&inputs.dockerfile);
        let dockerignore = parse::DockerIgnore::load(context);

        let mut files = Vec::new();

        if dockerfile.is_file() {
            files.push(ResolvedFile::Input(dockerfile.clone()));
        }

        for src in &parse::dockerfile_sources(&dockerfile, context, &inputs.build_args)? {
            for path in parse::expand_path(src, &dockerignore) {
                files.push(ResolvedFile::Input(path));
            }
        }

        Ok(files)
    }

    fn plan(&self, inputs: &ImageInputs, prior_state: Option<&ImageState>) -> Result<PlanResult, BoxError> {
        if inputs.platform.len() > 1 && !inputs.tag.contains('/') {
            return Err(format!(
                "multi-platform builds require a registry-qualified tag (e.g. \"registry.example.com/app:latest\"), got \"{}\"",
                inputs.tag
            ).into());
        }

        let args = build_args(inputs);
        let desc = format!("docker {}", args.join(" "));

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

        if prior.platform != inputs.platform {
            return Ok(PlanResult {
                action: PlanAction::Update,
                description: desc,
                reason: Some("platform changed".into()),
            });
        }

        let exists = Command::new("docker")
            .args(["image", "inspect", &prior.image_id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if !exists {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description: desc,
                reason: Some("image deleted".into()),
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
        inputs: &ImageInputs,
        _prior_state: Option<&ImageState>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<ImageState, ImageOutputs>, BoxError> {
        let args = build_args(inputs);
        let mut cmd = Command::new("docker");
        cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to run docker buildx build: {e}"))?;

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

        let status = child.wait().map_err(|e| format!("docker buildx build failed: {e}"))?;
        if !status.success() {
            return Err(format!("docker buildx build exited with {status}").into());
        }

        let digest_output = Command::new("docker")
            .args(["inspect", "--format", "{{.Id}}", &inputs.tag])
            .output()
            .map_err(|e| format!("docker inspect failed: {e}"))?;

        let image_id = String::from_utf8_lossy(&digest_output.stdout).trim().to_owned();

        Ok(ApplyResult {
            state: Some(ImageState {
                tag: inputs.tag.clone(),
                image_id: image_id.clone(),
                platform: inputs.platform.clone(),
            }),
            outputs: ImageOutputs {
                image_ref: inputs.tag.clone(),
                image_id,
            },
        })
    }

    fn destroy(&self, prior_state: &ImageState, writer: &BlockWriter) -> Result<(), BoxError> {
        use crate::output::Event;
        writer.event(Event::Starting, &format!("docker rmi -f {}", prior_state.image_id));
        let output = Command::new("docker")
            .args(["rmi", "-f", &prior_state.image_id])
            .output()
            .map_err(|e| format!("docker rmi failed: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(stderr.into());
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
                image_id: image_id.clone(),
            },
            state: Some(ImageState {
                tag: prior_state.tag.clone(),
                image_id,
                platform: prior_state.platform.clone(),
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ResolvedFile;

    #[test]
    fn plan_create_when_no_state() {
        let inputs = ImageInputs {
            tag: "myapp:latest".into(),
            context: ".".into(),
            dockerfile: "Dockerfile".into(),
            build_args: HashMap::new(),
            platform: vec![],
        };
        let result = Resource::plan(&ImageResource, &inputs, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert!(result.description.contains("myapp:latest"));
    }

    #[test]
    fn plan_create_when_image_deleted() {
        let inputs = ImageInputs {
            tag: "myapp:latest".into(),
            context: ".".into(),
            dockerfile: "Dockerfile".into(),
            build_args: HashMap::new(),
            platform: vec![],
        };
        let prior = ImageState {
            tag: "myapp:latest".into(),
            image_id: "sha256:nonexistent".into(),
            platform: vec![],
        };
        let result = Resource::plan(&ImageResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn plan_update_when_tag_changed() {
        let inputs = ImageInputs {
            tag: "myapp:v2".into(),
            context: ".".into(),
            dockerfile: "Dockerfile".into(),
            build_args: HashMap::new(),
            platform: vec![],
        };
        let prior = ImageState {
            tag: "myapp:v1".into(),
            image_id: "sha256:abc".into(),
            platform: vec![],
        };
        let result = Resource::plan(&ImageResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn plan_update_when_platform_changed() {
        let inputs = ImageInputs {
            tag: "myapp:latest".into(),
            context: ".".into(),
            dockerfile: "Dockerfile".into(),
            build_args: HashMap::new(),
            platform: vec!["linux/arm64".into()],
        };
        let prior = ImageState {
            tag: "myapp:latest".into(),
            image_id: "sha256:abc".into(),
            platform: vec!["linux/amd64".into()],
        };
        let result = Resource::plan(&ImageResource, &inputs, Some(&prior)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
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
            platform: vec![],
        };
        let resolved = Resource::resolve(&ImageResource, &inputs).unwrap();
        assert_eq!(resolved.len(), 2);
        assert!(resolved.contains(&ResolvedFile::Input(dockerfile)));
        assert!(resolved.contains(&ResolvedFile::Input(src_file)));
    }

    #[test]
    fn resolve_respects_dockerignore() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        let src_dir = dir.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(&dockerfile, "FROM alpine\nCOPY src/ /app/src/\n").unwrap();
        std::fs::write(src_dir.join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(src_dir.join("test.log"), "log output").unwrap();

        let inputs = ImageInputs {
            tag: "myapp:latest".into(),
            context: dir.path().to_string_lossy().into_owned(),
            dockerfile: "Dockerfile".into(),
            build_args: HashMap::new(),
            platform: vec![],
        };
        let resolved = Resource::resolve(&ImageResource, &inputs).unwrap();
        assert_eq!(resolved.len(), 3); // Dockerfile + main.rs + test.log

        std::fs::write(dir.path().join(".dockerignore"), "*.log\n").unwrap();
        let resolved = Resource::resolve(&ImageResource, &inputs).unwrap();
        assert_eq!(resolved.len(), 2); // Dockerfile + main.rs
        assert!(resolved.contains(&ResolvedFile::Input(dockerfile)));
        assert!(resolved.contains(&ResolvedFile::Input(src_dir.join("main.rs"))));
    }
}
