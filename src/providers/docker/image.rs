use std::collections::{BTreeMap, HashMap};
use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{ApplyResult, BoxError, PlanAction, PlanResult, Resource, ResourceKind};
use crate::sha256::SHA256;

use super::parse;

/// Build a Docker image (auto-detects inputs from Dockerfile)
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct ImageInputs {
    /// Image tag
    pub tag: String,
    /// Build context directory
    #[serde(default = "default_context")]
    pub context: String,
    /// Dockerfile path
    #[serde(default = "default_dockerfile")]
    pub dockerfile: String,
    /// Docker build arguments
    #[serde(default)]
    pub build_args: HashMap<String, String>,
    /// Target platform(s)
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

#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct ImageOutputs {
    /// Image tag/reference
    #[serde(rename = "ref")]
    pub image_ref: String,
    /// Docker image ID
    pub image_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageState {
    pub tag: String,
    pub image_id: String,
    #[serde(default)]
    pub platform: Vec<String>,
    #[serde(default)]
    pub pinned_tag: Option<String>,
}

fn strip_docker_prefix(id: &str) -> &str {
    id.strip_prefix("sha256:").unwrap_or(id)
}

fn pinned_tag(tag: &str, image_id: &str) -> String {
    let name = tag.split(':').next().unwrap_or(tag);
    format!("{name}:{image_id}")
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

    fn resolve(&self, inputs: &ImageInputs, tracker: &mut FileTracker) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let context = Path::new(&inputs.context);
        let dockerfile = context.join(&inputs.dockerfile);
        let dockerignore = parse::DockerIgnore::load(context);

        let mut files = BTreeMap::new();

        if dockerfile.is_file() {
            let hash = tracker.hash_file(&dockerfile)?;
            files.insert(dockerfile.to_string_lossy().into_owned(), hash);
        }

        for src in &parse::dockerfile_sources(&dockerfile, context, &inputs.build_args)? {
            for path in parse::expand_path(src, &dockerignore) {
                let hash = tracker.hash_file(&path)?;
                files.insert(path.to_string_lossy().into_owned(), hash);
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

        let raw_id = String::from_utf8_lossy(&digest_output.stdout).trim().to_owned();
        let image_id = strip_docker_prefix(&raw_id).to_owned();

        let pinned = pinned_tag(&inputs.tag, &image_id);
        let tag_status = Command::new("docker")
            .args(["tag", &inputs.tag, &pinned])
            .output()
            .map_err(|e| format!("docker tag failed: {e}"))?;
        if !tag_status.status.success() {
            return Err(format!(
                "docker tag failed: {}",
                String::from_utf8_lossy(&tag_status.stderr).trim()
            )
            .into());
        }

        Ok(ApplyResult {
            state: Some(ImageState {
                tag: inputs.tag.clone(),
                image_id: image_id.clone(),
                platform: inputs.platform.clone(),
                pinned_tag: Some(pinned.clone()),
            }),
            outputs: ImageOutputs {
                image_ref: pinned,
                image_id,
            },
        })
    }

    fn destroy(&self, prior_state: &ImageState, writer: &BlockWriter) -> Result<(), BoxError> {
        use crate::output::Event;
        if let Some(pinned) = &prior_state.pinned_tag {
            writer.event(Event::Starting, &format!("docker rmi -f {pinned}"));
            let _ = Command::new("docker").args(["rmi", "-f", pinned]).output();
        }
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
            image_id: "nonexistent".into(),
            platform: vec![],
            pinned_tag: None,
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
            image_id: "abc".into(),
            platform: vec![],
            pinned_tag: None,
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
            image_id: "abc".into(),
            platform: vec!["linux/amd64".into()],
            pinned_tag: None,
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
        let mut tracker = FileTracker::default();
        let resolved = Resource::resolve(&ImageResource, &inputs, &mut tracker).unwrap();
        assert_eq!(resolved.len(), 2);
        assert!(resolved.contains_key(&dockerfile.to_string_lossy().into_owned()));
        assert!(resolved.contains_key(&src_file.to_string_lossy().into_owned()));
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
        let mut tracker = FileTracker::default();
        let resolved = Resource::resolve(&ImageResource, &inputs, &mut tracker).unwrap();
        assert_eq!(resolved.len(), 3); // Dockerfile + main.rs + test.log

        std::fs::write(dir.path().join(".dockerignore"), "*.log\n").unwrap();
        let mut tracker = FileTracker::default();
        let resolved = Resource::resolve(&ImageResource, &inputs, &mut tracker).unwrap();
        assert_eq!(resolved.len(), 2); // Dockerfile + main.rs
        assert!(resolved.contains_key(&dockerfile.to_string_lossy().into_owned()));
        assert!(resolved.contains_key(&src_dir.join("main.rs").to_string_lossy().into_owned()));
    }

    #[test]
    fn pinned_tag_uses_full_digest() {
        assert_eq!(
            pinned_tag("myapp:latest", "fe98a05f929ea35f5aae13cc82f9bd3b"),
            "myapp:fe98a05f929ea35f5aae13cc82f9bd3b"
        );
    }

    #[test]
    fn pinned_tag_handles_no_existing_tag() {
        assert_eq!(pinned_tag("myapp", "abcdef123456789"), "myapp:abcdef123456789");
    }

    #[test]
    fn pinned_tag_handles_registry_prefix() {
        assert_eq!(
            pinned_tag("registry.example.com/app:v1", "abcdef123456789"),
            "registry.example.com/app:abcdef123456789"
        );
    }

    #[test]
    fn strip_docker_prefix_removes_sha256() {
        assert_eq!(strip_docker_prefix("sha256:abc123"), "abc123");
        assert_eq!(strip_docker_prefix("abc123"), "abc123");
    }
}
