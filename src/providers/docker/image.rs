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
    pub image_id: String,
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

/// Parse a .dockerignore file into ignore/negate pattern lists.
fn load_dockerignore(context: &Path) -> Option<DockerIgnore> {
    let contents = std::fs::read_to_string(context.join(".dockerignore")).ok()?;
    let mut ignore = Vec::new();
    let mut negate = Vec::new();

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (pattern, is_negation) = if let Some(rest) = line.strip_prefix('!') {
            (rest.trim(), true)
        } else {
            (line, false)
        };

        // Patterns without a slash match anywhere in the tree
        let glob_pattern = if pattern.contains('/') {
            pattern.to_owned()
        } else {
            format!("**/{pattern}")
        };

        let Ok(pat) = glob::Pattern::new(&glob_pattern) else {
            continue;
        };
        // Also match contents of directories (e.g. "src/build" also matches "src/build/**")
        let dir_pat = glob::Pattern::new(&format!("{glob_pattern}/**")).ok();
        if is_negation {
            negate.push(pat);
            if let Some(dp) = dir_pat {
                negate.push(dp);
            }
        } else {
            ignore.push(pat);
            if let Some(dp) = dir_pat {
                ignore.push(dp);
            }
        }
    }

    Some(DockerIgnore {
        context: context.to_path_buf(),
        ignore,
        negate,
    })
}

struct DockerIgnore {
    context: PathBuf,
    ignore: Vec<glob::Pattern>,
    negate: Vec<glob::Pattern>,
}

impl DockerIgnore {
    fn is_ignored(&self, path: &Path) -> bool {
        // Match against the path relative to the build context
        let rel = path.strip_prefix(&self.context).unwrap_or(path);
        self.ignore.iter().any(|p| p.matches_path(rel)) && !self.negate.iter().any(|p| p.matches_path(rel))
    }
}

/// Expand a path (file, directory, or glob) into individual files,
/// filtering out paths matched by .dockerignore.
fn expand_path(path: &Path, dockerignore: &Option<DockerIgnore>) -> Vec<PathBuf> {
    let pattern = path.to_string_lossy();
    let filter = |e: &PathBuf| -> bool { e.is_file() && !dockerignore.as_ref().is_some_and(|di| di.is_ignored(e)) };

    if path.is_dir() {
        let glob_pattern = format!("{}/**/*", pattern);
        if let Ok(entries) = glob::glob(&glob_pattern) {
            return entries.flatten().filter(|e| filter(e)).collect();
        }
        return vec![];
    }

    if let Ok(entries) = glob::glob(&pattern) {
        return entries.flatten().filter(|e| filter(e)).collect();
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

    fn resolve(&self, inputs: &ImageInputs) -> Result<Vec<crate::provider::ResolvedFile>, BoxError> {
        use crate::provider::ResolvedFile;
        let context = Path::new(&inputs.context);
        let dockerfile = context.join(&inputs.dockerfile);
        let dockerignore = load_dockerignore(context);

        let mut files = Vec::new();

        // Include the Dockerfile itself (never ignored)
        if dockerfile.is_file() {
            files.push(ResolvedFile::Input(dockerfile.clone()));
        }

        // Parse Dockerfile to discover COPY/ADD source paths, filtered by .dockerignore
        for src in &parse_dockerfile_sources(&dockerfile, context) {
            for path in expand_path(src, &dockerignore) {
                files.push(ResolvedFile::Input(path));
            }
        }

        Ok(files)
    }

    fn plan(&self, inputs: &ImageInputs, prior_state: Option<&ImageState>) -> Result<PlanResult, BoxError> {
        let Some(prior) = prior_state else {
            return Ok(PlanResult {
                action: PlanAction::Create,
                description: format!("docker build -t {}", inputs.tag),
                reason: None,
            });
        };

        let desc = format!("docker build -t {}", inputs.tag);

        if prior.tag != inputs.tag {
            return Ok(PlanResult {
                action: PlanAction::Update,
                description: desc.clone(),
                reason: Some("tag changed".into()),
            });
        }

        // Check if the image still exists
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
            description: format!("docker build -t {}", inputs.tag),
                reason: None,
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
            state: Some(ImageState {
                tag: inputs.tag.clone(),
                image_id: image_id.clone(),
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
        };
        let prior = ImageState {
            tag: "myapp:latest".into(),
            image_id: "sha256:nonexistent".into(),
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
        };
        let prior = ImageState {
            tag: "myapp:v1".into(),
            image_id: "sha256:abc".into(),
        };
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
        assert_eq!(resolved.len(), 2);
        assert!(resolved.contains(&ResolvedFile::Input(dockerfile)));
        assert!(resolved.contains(&ResolvedFile::Input(src_file)));
    }

    #[test]
    fn resolve_respects_dockerignore() {
        use crate::provider::ResolvedFile;
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        let src_dir = dir.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(&dockerfile, "FROM alpine\nCOPY src/ /app/src/\n").unwrap();
        std::fs::write(src_dir.join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(src_dir.join("test.log"), "log output").unwrap();

        // Without .dockerignore, both files are included
        let inputs = ImageInputs {
            tag: "myapp:latest".into(),
            context: dir.path().to_string_lossy().into_owned(),
            dockerfile: "Dockerfile".into(),
            build_args: HashMap::new(),
        };
        let resolved = Resource::resolve(&ImageResource, &inputs).unwrap();
        assert_eq!(resolved.len(), 3); // Dockerfile + main.rs + test.log

        // Add .dockerignore to exclude .log files
        std::fs::write(dir.path().join(".dockerignore"), "*.log\n").unwrap();
        let resolved = Resource::resolve(&ImageResource, &inputs).unwrap();
        assert_eq!(resolved.len(), 2); // Dockerfile + main.rs
        assert!(resolved.contains(&ResolvedFile::Input(dockerfile)));
        assert!(resolved.contains(&ResolvedFile::Input(src_dir.join("main.rs"))));
    }

    #[test]
    fn dockerignore_negation() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        let src_dir = dir.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(&dockerfile, "FROM alpine\nCOPY src/ /app/\n").unwrap();
        std::fs::write(src_dir.join("a.log"), "log").unwrap();
        std::fs::write(src_dir.join("important.log"), "keep").unwrap();
        std::fs::write(src_dir.join("main.rs"), "fn main() {}").unwrap();

        // Ignore all .log except important.log
        std::fs::write(dir.path().join(".dockerignore"), "*.log\n!important.log\n").unwrap();

        let inputs = ImageInputs {
            tag: "test:latest".into(),
            context: dir.path().to_string_lossy().into_owned(),
            dockerfile: "Dockerfile".into(),
            build_args: HashMap::new(),
        };
        let resolved = Resource::resolve(&ImageResource, &inputs).unwrap();
        // Dockerfile + main.rs + important.log (a.log excluded)
        assert_eq!(resolved.len(), 3);
        assert!(resolved.contains(&ResolvedFile::Input(src_dir.join("main.rs"))));
        assert!(resolved.contains(&ResolvedFile::Input(src_dir.join("important.log"))));
        assert!(!resolved.contains(&ResolvedFile::Input(src_dir.join("a.log"))));
    }

    #[test]
    fn dockerignore_comments_and_blank_lines() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        let src_dir = dir.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(&dockerfile, "FROM alpine\nCOPY src/ /app/\n").unwrap();
        std::fs::write(src_dir.join("keep.txt"), "keep").unwrap();
        std::fs::write(src_dir.join("drop.tmp"), "drop").unwrap();

        std::fs::write(
            dir.path().join(".dockerignore"),
            "# This is a comment\n\n*.tmp\n\n# Another comment\n",
        )
        .unwrap();

        let inputs = ImageInputs {
            tag: "test:latest".into(),
            context: dir.path().to_string_lossy().into_owned(),
            dockerfile: "Dockerfile".into(),
            build_args: HashMap::new(),
        };
        let resolved = Resource::resolve(&ImageResource, &inputs).unwrap();
        assert_eq!(resolved.len(), 2); // Dockerfile + keep.txt
        assert!(resolved.contains(&ResolvedFile::Input(src_dir.join("keep.txt"))));
        assert!(!resolved.contains(&ResolvedFile::Input(src_dir.join("drop.tmp"))));
    }

    #[test]
    fn dockerignore_subdirectory_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        let src_dir = dir.path().join("src");
        let build_dir = dir.path().join("src").join("build");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::write(&dockerfile, "FROM alpine\nCOPY src/ /app/\n").unwrap();
        std::fs::write(src_dir.join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(build_dir.join("output.o"), "binary").unwrap();

        // Ignore the build subdirectory by path
        std::fs::write(dir.path().join(".dockerignore"), "src/build\n").unwrap();

        let inputs = ImageInputs {
            tag: "test:latest".into(),
            context: dir.path().to_string_lossy().into_owned(),
            dockerfile: "Dockerfile".into(),
            build_args: HashMap::new(),
        };
        let resolved = Resource::resolve(&ImageResource, &inputs).unwrap();
        assert_eq!(resolved.len(), 2); // Dockerfile + main.rs
        assert!(resolved.contains(&ResolvedFile::Input(src_dir.join("main.rs"))));
        assert!(!resolved.contains(&ResolvedFile::Input(build_dir.join("output.o"))));
    }

    #[test]
    fn dockerignore_matches_in_nested_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        let deep = dir.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(&dockerfile, "FROM alpine\nCOPY a/ /app/\n").unwrap();
        std::fs::write(deep.join("data.txt"), "data").unwrap();
        std::fs::write(deep.join("cache.tmp"), "cache").unwrap();

        // Pattern without slash matches anywhere in the tree
        std::fs::write(dir.path().join(".dockerignore"), "*.tmp\n").unwrap();

        let inputs = ImageInputs {
            tag: "test:latest".into(),
            context: dir.path().to_string_lossy().into_owned(),
            dockerfile: "Dockerfile".into(),
            build_args: HashMap::new(),
        };
        let resolved = Resource::resolve(&ImageResource, &inputs).unwrap();
        assert_eq!(resolved.len(), 2); // Dockerfile + data.txt
        assert!(resolved.contains(&ResolvedFile::Input(deep.join("data.txt"))));
        assert!(!resolved.contains(&ResolvedFile::Input(deep.join("cache.tmp"))));
    }
}
