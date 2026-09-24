use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::cache::{ArtifactRef, Cas};
use crate::file_tracker::FileTracker;
use crate::output::BlockWriter;
use crate::provider::{
    ApplyResult, BoxError, CachePolicy, MaterializeResult, OutputFile, PlanAction, PlanResult, ReceiptCheck, Resource,
    ResourceKind,
};
use crate::sha256::SHA256;

use super::parse;

const MANIFEST_ROLE: &str = "manifest.json";

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
    /// Locally pinned tag or registry digest reference
    #[serde(rename = "ref")]
    pub image_ref: String,
    /// Docker image ID or multi-platform manifest digest, without `sha256:`
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

fn repository_name(image: &str) -> &str {
    let image = image.split_once('@').map_or(image, |(name, _)| name);
    match (image.rfind('/'), image.rfind(':')) {
        (slash, Some(colon)) if slash.is_none_or(|slash| colon > slash) => &image[..colon],
        _ => image,
    }
}

fn pinned_tag(tag: &str, image_id: &str) -> String {
    format!("{}:{image_id}", repository_name(tag))
}

fn digest_reference(tag: &str, digest: &str) -> Result<String, BoxError> {
    let Some((algorithm, encoded)) = digest.split_once(':') else {
        return Err(format!("invalid Buildx image digest {digest:?}").into());
    };
    if algorithm.is_empty()
        || !algorithm
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'+' | b'.' | b'_' | b'-'))
        || encoded.is_empty()
        || !encoded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'=' | b'_' | b'-'))
    {
        return Err(format!("invalid Buildx image digest {digest:?}").into());
    }
    Ok(format!("{}@{digest}", repository_name(tag)))
}

#[derive(Deserialize)]
struct BuildMetadata {
    #[serde(rename = "containerimage.digest")]
    image_digest: Option<String>,
}

fn read_build_digest(path: &Path) -> Result<String, BoxError> {
    let file = File::open(path).map_err(|e| format!("failed to read Buildx metadata: {e}"))?;
    let metadata: BuildMetadata =
        serde_json::from_reader(file).map_err(|e| format!("failed to parse Buildx metadata: {e}"))?;
    let digest = metadata
        .image_digest
        .ok_or("Buildx metadata did not contain containerimage.digest")?;
    digest_reference("image", &digest)?;
    Ok(digest)
}

fn archive_member_path(role: &str) -> io::Result<&Path> {
    let path = Path::new(role);
    let has_unsafe_segment = role
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..");
    if has_unsafe_segment
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsafe Docker archive member path {role:?}"),
        ));
    }
    Ok(path)
}

/// Split a Docker image archive into independently addressable members. The
/// archive path is the artifact role, so the receipt contains everything
/// needed to reconstruct the archive while the CAS deduplicates shared layers.
fn capture_archive_members(archive_path: &Path, cas: &Cas) -> Result<BTreeMap<String, ArtifactRef>, BoxError> {
    let file = File::open(archive_path)?;
    let mut archive = tar::Archive::new(BufReader::new(file));
    let mut artifacts = BTreeMap::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            continue;
        }
        if !entry_type.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported Docker archive member type for {}", entry.path()?.display()),
            )
            .into());
        }

        let member_path = entry.path()?.into_owned();
        let role = archive_member_path(
            member_path
                .to_str()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 Docker archive member path"))?,
        )?
        .to_string_lossy()
        .into_owned();
        if artifacts.contains_key(&role) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("duplicate Docker archive member {role:?}"),
            )
            .into());
        }

        let mode = entry.header().mode()? & 0o777;
        let mut member = tempfile::NamedTempFile::new()?;
        io::copy(&mut entry, member.as_file_mut())?;
        member.as_file().sync_all()?;
        fs::set_permissions(member.path(), fs::Permissions::from_mode(mode))?;
        artifacts.insert(role, cas.put_file(member.path())?);
    }
    if !artifacts.contains_key(MANIFEST_ROLE) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Docker archive has no manifest.json").into());
    }
    Ok(artifacts)
}

/// Reassemble cached Docker archive members into a loadable tar file.
fn materialize_archive(
    artifacts: &BTreeMap<String, ArtifactRef>,
    cas: &Cas,
    archive_path: &Path,
) -> Result<(), BoxError> {
    if !artifacts.contains_key(MANIFEST_ROLE) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "receipt has no Docker manifest").into());
    }

    let members = tempfile::tempdir()?;
    let mut materialized = Vec::with_capacity(artifacts.len());
    for (role, artifact) in artifacts {
        let relative = archive_member_path(role)?;
        let path = members.path().join(relative);
        cas.materialize(artifact, &path)?;
        materialized.push((relative.to_path_buf(), path));
    }

    let file = File::create(archive_path)?;
    let mut archive = tar::Builder::new(file);
    for (relative, path) in materialized {
        archive.append_path_with_name(path, relative)?;
    }
    archive.into_inner()?.sync_all()?;
    Ok(())
}

/// Build the argument list for `docker buildx build`.
fn build_args(inputs: &ImageInputs, metadata_path: Option<&Path>) -> Vec<String> {
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
    if let Some(path) = metadata_path {
        args.push("--metadata-file".into());
        args.push(path.to_string_lossy().into_owned());
    }
    args.push(inputs.context.clone());
    args
}

pub struct ImageResource {
    tracker: Arc<Mutex<FileTracker>>,
    docker: PathBuf,
}

impl ImageResource {
    pub(super) fn new(tracker: Arc<Mutex<FileTracker>>) -> Self {
        Self {
            tracker,
            docker: "docker".into(),
        }
    }

    fn command(&self) -> Command {
        Command::new(&self.docker)
    }

    fn inspect_image_id_result(&self, image: &str) -> Result<String, BoxError> {
        let output = self
            .command()
            .args(["image", "inspect", "--format", "{{.Id}}", image])
            .output()
            .map_err(|e| format!("failed to run docker image inspect: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "docker image inspect failed for {image}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
            .into());
        }
        let raw_id = String::from_utf8_lossy(&output.stdout);
        let image_id = strip_docker_prefix(raw_id.trim());
        if image_id.is_empty() {
            return Err(format!("docker image inspect returned no image ID for {image}").into());
        }
        Ok(image_id.to_owned())
    }

    fn inspect_image_id(&self, image: &str) -> Option<String> {
        self.inspect_image_id_result(image).ok()
    }

    fn remote_image_exists(&self, image: &str) -> bool {
        self.command()
            .args(["buildx", "imagetools", "inspect", image])
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn tag_image(&self, image: &str, tag: &str) -> Result<(), BoxError> {
        let output = self
            .command()
            .args(["tag", image, tag])
            .output()
            .map_err(|e| format!("failed to run docker tag: {e}"))?;
        if !output.status.success() {
            return Err(format!("docker tag failed: {}", String::from_utf8_lossy(&output.stderr).trim()).into());
        }
        Ok(())
    }

    fn remove_image(&self, image: &str) -> Result<(), BoxError> {
        let output = self
            .command()
            .args(["rmi", "-f", image])
            .output()
            .map_err(|e| format!("docker rmi failed: {e}"))?;
        super::check_remove_output("docker rmi", output)
    }
}

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

    fn resolve(&self, inputs: &ImageInputs) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let context = Path::new(&inputs.context);
        let dockerfile = context.join(&inputs.dockerfile);
        let dockerignore = parse::DockerIgnore::load(context);

        let mut tracker = self.tracker.lock().expect("tracker lock poisoned");
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

        let args = build_args(inputs, None);
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

        let exists = if inputs.platform.len() > 1 {
            let digest = format!("sha256:{}", prior.image_id);
            let image_ref = prior
                .pinned_tag
                .clone()
                .filter(|image_ref| image_ref.contains('@'))
                .or_else(|| digest_reference(&prior.tag, &digest).ok());
            image_ref.is_some_and(|image_ref| self.remote_image_exists(&image_ref))
        } else {
            self.inspect_image_id(&prior.image_id).as_deref() == Some(prior.image_id.as_str())
        };

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
        let metadata = (inputs.platform.len() > 1)
            .then(tempfile::NamedTempFile::new)
            .transpose()
            .map_err(|e| format!("failed to create Buildx metadata file: {e}"))?;
        let args = build_args(inputs, metadata.as_ref().map(|file| file.path()));
        let mut cmd = self.command();
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

        // A multi-platform push has no local image ID. Buildx metadata is the
        // authoritative digest of the manifest list that was just pushed.
        let (image_id, pinned) = if let Some(metadata) = metadata {
            let digest = read_build_digest(metadata.path())?;
            (
                strip_docker_prefix(&digest).to_owned(),
                digest_reference(&inputs.tag, &digest)?,
            )
        } else {
            let image_id = self.inspect_image_id_result(&inputs.tag)?;
            let pinned = pinned_tag(&inputs.tag, &image_id);
            self.tag_image(&inputs.tag, &pinned)?;
            (image_id, pinned)
        };

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
        // Multi-platform builds never enter the local image store, and this
        // build resource does not own deletion from an external registry.
        if prior_state.platform.len() > 1 {
            return Ok(());
        }
        if let Some(pinned) = &prior_state.pinned_tag {
            writer.event(Event::Starting, &format!("docker rmi -f {pinned}"));
            self.remove_image(pinned)?;
        }
        writer.event(Event::Starting, &format!("docker rmi -f {}", prior_state.image_id));
        self.remove_image(&prior_state.image_id)
    }

    fn cache_policy(&self, inputs: &ImageInputs) -> CachePolicy {
        // A registry push cannot be reconstructed from Bit's local CAS or
        // vouched for by another worktree's receipt.
        if inputs.platform.len() > 1 {
            CachePolicy::Local
        } else {
            CachePolicy::Shared { version: 2 }
        }
    }

    fn capture_artifacts(
        &self,
        inputs: &ImageInputs,
        state: &ImageState,
        _outputs: &[OutputFile],
        cas: &Cas,
    ) -> Result<BTreeMap<String, ArtifactRef>, BoxError> {
        if inputs.platform.len() > 1 {
            return Err(
                "multi-platform images are pushed to a registry and cannot be saved from the local image store".into(),
            );
        }
        let image = state
            .pinned_tag
            .clone()
            .unwrap_or_else(|| pinned_tag(&state.tag, &state.image_id));
        let archive = tempfile::NamedTempFile::new()?;
        let output = self
            .command()
            .args(["image", "save", "--output"])
            .arg(archive.path())
            .arg(&image)
            .output()
            .map_err(|e| format!("failed to run docker image save: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "docker image save failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
            .into());
        }
        capture_archive_members(archive.path(), cas)
    }

    fn check_receipt(
        &self,
        inputs: &ImageInputs,
        state: &ImageState,
        _outputs: &[OutputFile],
        artifacts: &BTreeMap<String, ArtifactRef>,
        _cas: &Cas,
    ) -> Result<ReceiptCheck, BoxError> {
        if inputs.platform.len() > 1 || !artifacts.contains_key(MANIFEST_ROLE) {
            return Ok(ReceiptCheck::Unusable);
        }
        let pinned = pinned_tag(&inputs.tag, &state.image_id);
        Ok(
            if self.inspect_image_id(&pinned).as_deref() == Some(state.image_id.as_str()) {
                ReceiptCheck::Valid
            } else {
                ReceiptCheck::Restore
            },
        )
    }

    fn materialize(
        &self,
        inputs: &ImageInputs,
        state: &ImageState,
        _outputs: &[OutputFile],
        artifacts: &BTreeMap<String, ArtifactRef>,
        cas: &Cas,
        writer: &BlockWriter,
    ) -> MaterializeResult<ImageState, ImageOutputs> {
        if inputs.platform.len() > 1 {
            return Err("multi-platform image receipts cannot be materialized from the local image store".into());
        }
        let pinned = pinned_tag(&inputs.tag, &state.image_id);

        if self.inspect_image_id(&state.image_id).as_deref() != Some(state.image_id.as_str()) {
            let dir = tempfile::tempdir()?;
            let archive = dir.path().join("image.tar");
            materialize_archive(artifacts, cas, &archive)?;
            writer.line(&format!("docker image load --input {}", archive.display()));
            let output = self
                .command()
                .args(["image", "load", "--input"])
                .arg(&archive)
                .output()
                .map_err(|e| format!("failed to run docker image load: {e}"))?;
            if !output.status.success() {
                return Err(format!(
                    "docker image load failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )
                .into());
            }
        }

        if self.inspect_image_id(&pinned).as_deref() != Some(state.image_id.as_str()) {
            self.tag_image(&state.image_id, &pinned)?;
        }
        if self.inspect_image_id(&inputs.tag).as_deref() != Some(state.image_id.as_str()) {
            self.tag_image(&state.image_id, &inputs.tag)?;
        }

        Ok(Some(ApplyResult {
            state: Some(ImageState {
                tag: inputs.tag.clone(),
                image_id: state.image_id.clone(),
                platform: inputs.platform.clone(),
                pinned_tag: Some(pinned.clone()),
            }),
            outputs: ImageOutputs {
                image_ref: pinned,
                image_id: state.image_id.clone(),
            },
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn write_archive(path: &Path, members: &[(&str, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut archive = tar::Builder::new(file);
        for (name, contents) in members {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append_data(&mut header, name, *contents).unwrap();
        }
        archive.finish().unwrap();
    }

    fn read_archive(path: &Path) -> BTreeMap<String, Vec<u8>> {
        let file = File::open(path).unwrap();
        let mut archive = tar::Archive::new(file);
        archive
            .entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                let path = entry.path().unwrap().to_string_lossy().into_owned();
                let mut contents = Vec::new();
                entry.read_to_end(&mut contents).unwrap();
                (path, contents)
            })
            .collect()
    }

    fn test_inputs() -> ImageInputs {
        ImageInputs {
            tag: "myapp:latest".into(),
            context: ".".into(),
            dockerfile: "Dockerfile".into(),
            build_args: HashMap::new(),
            platform: vec![],
        }
    }

    fn test_state() -> ImageState {
        ImageState {
            tag: "myapp:latest".into(),
            image_id: "abc123".into(),
            platform: vec![],
            pinned_tag: Some("myapp:abc123".into()),
        }
    }

    fn multi_platform_inputs() -> ImageInputs {
        ImageInputs {
            tag: "registry.example.com:5000/team/myapp:v1".into(),
            context: ".".into(),
            dockerfile: "Dockerfile".into(),
            build_args: HashMap::new(),
            platform: vec!["linux/amd64".into(), "linux/arm64".into()],
        }
    }

    #[test]
    fn plan_create_when_no_state() {
        let inputs = test_inputs();
        let result = Resource::plan(
            &ImageResource::new(Arc::new(Mutex::new(FileTracker::default()))),
            &inputs,
            None,
        )
        .unwrap();
        assert_eq!(result.action, PlanAction::Create);
        assert!(result.description.contains("myapp:latest"));
    }

    #[test]
    fn destroy_removes_pinned_tag_and_image() {
        let dir = tempfile::tempdir().unwrap();
        let docker = dir.path().join("docker");
        std::fs::write(
            &docker,
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$(dirname "$0")/docker.log"
exit 0
"#,
        )
        .unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let resource = ImageResource {
            tracker: Arc::new(Mutex::new(FileTracker::default())),
            docker,
        };
        let output = crate::output::Output::new(&[]);

        Resource::destroy(&resource, &test_state(), &output.writer("image")).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("docker.log")).unwrap(),
            "rmi -f myapp:abc123\nrmi -f abc123\n"
        );
    }

    #[test]
    fn plan_create_when_image_deleted() {
        let inputs = test_inputs();
        let prior = ImageState {
            tag: "myapp:latest".into(),
            image_id: "nonexistent".into(),
            platform: vec![],
            pinned_tag: None,
        };
        let result = Resource::plan(
            &ImageResource::new(Arc::new(Mutex::new(FileTracker::default()))),
            &inputs,
            Some(&prior),
        )
        .unwrap();
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
        let result = Resource::plan(
            &ImageResource::new(Arc::new(Mutex::new(FileTracker::default()))),
            &inputs,
            Some(&prior),
        )
        .unwrap();
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
        let result = Resource::plan(
            &ImageResource::new(Arc::new(Mutex::new(FileTracker::default()))),
            &inputs,
            Some(&prior),
        )
        .unwrap();
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
        let resolved = Resource::resolve(
            &ImageResource::new(Arc::new(Mutex::new(FileTracker::default()))),
            &inputs,
        )
        .unwrap();
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
        let resolved = Resource::resolve(
            &ImageResource::new(Arc::new(Mutex::new(FileTracker::default()))),
            &inputs,
        )
        .unwrap();
        assert_eq!(resolved.len(), 3); // Dockerfile + main.rs + test.log

        std::fs::write(dir.path().join(".dockerignore"), "*.log\n").unwrap();
        let resolved = Resource::resolve(
            &ImageResource::new(Arc::new(Mutex::new(FileTracker::default()))),
            &inputs,
        )
        .unwrap();
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
    fn pinned_tag_preserves_registry_port() {
        assert_eq!(
            pinned_tag("registry.example.com:5000/app:v1", "abcdef123456789"),
            "registry.example.com:5000/app:abcdef123456789"
        );
    }

    #[test]
    fn digest_reference_replaces_tag_and_preserves_registry_port() {
        let digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            digest_reference("registry.example.com:5000/app:v1", &digest).unwrap(),
            format!("registry.example.com:5000/app@{digest}")
        );
        assert!(digest_reference("registry.example.com/app:v1", "").is_err());
        assert!(digest_reference("registry.example.com/app:v1", "sha256:").is_err());
    }

    #[test]
    fn build_metadata_requires_an_image_digest() {
        let metadata = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(metadata.path(), "{}").unwrap();

        let error = read_build_digest(metadata.path()).unwrap_err().to_string();

        assert!(error.contains("containerimage.digest"), "{error}");
    }

    #[test]
    fn strip_docker_prefix_removes_sha256() {
        assert_eq!(strip_docker_prefix("sha256:abc123"), "abc123");
        assert_eq!(strip_docker_prefix("abc123"), "abc123");
    }

    #[test]
    fn image_uses_shared_cache() {
        let resource = ImageResource::new(Arc::new(Mutex::new(FileTracker::default())));
        assert_eq!(
            Resource::cache_policy(&resource, &test_inputs()),
            CachePolicy::Shared { version: 2 }
        );
    }

    #[test]
    fn multi_platform_image_uses_local_state_only() {
        let resource = ImageResource::new(Arc::new(Mutex::new(FileTracker::default())));
        assert_eq!(
            Resource::cache_policy(&resource, &multi_platform_inputs()),
            CachePolicy::Local
        );
    }

    #[test]
    fn multi_platform_apply_uses_buildx_digest_without_local_tagging() {
        let dir = tempfile::tempdir().unwrap();
        let docker = dir.path().join("docker");
        std::fs::write(
            &docker,
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$(dirname "$0")/docker.log"
if [ "$1" = "buildx" ] && [ "$2" = "build" ]; then
  while [ "$#" -gt 0 ]; do
    if [ "$1" = "--metadata-file" ]; then
      printf '{"containerimage.digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}\n' > "$2"
      exit 0
    fi
    shift
  done
fi
exit 91
"#,
        )
        .unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let resource = ImageResource {
            tracker: Arc::new(Mutex::new(FileTracker::default())),
            docker,
        };
        let output = crate::output::Output::new(&[]);

        let result = Resource::apply(&resource, &multi_platform_inputs(), None, &output.writer("image")).unwrap();

        let digest = "a".repeat(64);
        let image_ref = format!("registry.example.com:5000/team/myapp@sha256:{digest}");
        assert_eq!(result.outputs.image_ref, image_ref);
        assert_eq!(result.outputs.image_id, digest);
        assert_eq!(result.state.unwrap().pinned_tag.as_deref(), Some(image_ref.as_str()));
        let log = std::fs::read_to_string(dir.path().join("docker.log")).unwrap();
        assert!(log.contains("--platform linux/amd64,linux/arm64 --push"), "{log}");
        assert!(log.contains("--metadata-file"), "{log}");
        assert!(!log.lines().any(|line| line.starts_with("tag ")), "{log}");
        assert!(!log.lines().any(|line| line.starts_with("image inspect ")), "{log}");
    }

    #[test]
    fn multi_platform_plan_checks_immutable_registry_reference() {
        let dir = tempfile::tempdir().unwrap();
        let docker = dir.path().join("docker");
        std::fs::write(
            &docker,
            r#"#!/bin/sh
printf '%s\n' "$*" > "$(dirname "$0")/docker.log"
[ "$1" = "buildx" ] && [ "$2" = "imagetools" ] && [ "$3" = "inspect" ]
"#,
        )
        .unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let resource = ImageResource {
            tracker: Arc::new(Mutex::new(FileTracker::default())),
            docker,
        };
        let digest = "b".repeat(64);
        let state = ImageState {
            tag: "registry.example.com:5000/team/myapp:v1".into(),
            image_id: digest.clone(),
            platform: vec!["linux/amd64".into(), "linux/arm64".into()],
            pinned_tag: Some(format!("registry.example.com:5000/team/myapp@sha256:{digest}")),
        };

        let plan = Resource::plan(&resource, &multi_platform_inputs(), Some(&state)).unwrap();

        assert_eq!(plan.action, PlanAction::None);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("docker.log")).unwrap(),
            format!("buildx imagetools inspect registry.example.com:5000/team/myapp@sha256:{digest}\n")
        );
    }

    #[test]
    fn multi_platform_destroy_does_not_remove_a_local_image() {
        let resource = ImageResource {
            tracker: Arc::new(Mutex::new(FileTracker::default())),
            docker: "/does/not/exist/docker".into(),
        };
        let state = ImageState {
            tag: "registry.example.com/team/myapp:v1".into(),
            image_id: "a".repeat(64),
            platform: vec!["linux/amd64".into(), "linux/arm64".into()],
            pinned_tag: Some(format!("registry.example.com/team/myapp@sha256:{}", "a".repeat(64))),
        };
        let output = crate::output::Output::new(&[]);

        Resource::destroy(&resource, &state, &output.writer("image")).unwrap();
    }

    #[test]
    fn single_platform_apply_reports_inspect_failure_without_tagging() {
        let dir = tempfile::tempdir().unwrap();
        let docker = dir.path().join("docker");
        std::fs::write(
            &docker,
            r#"#!/bin/sh
if [ "$1" = "buildx" ] && [ "$2" = "build" ]; then
  exit 0
fi
if [ "$1" = "image" ] && [ "$2" = "inspect" ]; then
  printf 'image unavailable\n' >&2
  exit 1
fi
if [ "$1" = "tag" ]; then
  touch "$(dirname "$0")/tagged"
  exit 0
fi
exit 91
"#,
        )
        .unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let resource = ImageResource {
            tracker: Arc::new(Mutex::new(FileTracker::default())),
            docker,
        };
        let output = crate::output::Output::new(&[]);

        let error = Resource::apply(&resource, &test_inputs(), None, &output.writer("image"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("docker image inspect failed"), "{error}");
        assert!(error.contains("image unavailable"), "{error}");
        assert!(!dir.path().join("tagged").exists());
    }

    #[test]
    fn receipt_without_image_archive_is_unusable() {
        let resource = ImageResource::new(Arc::new(Mutex::new(FileTracker::default())));
        assert_eq!(
            Resource::check_receipt(
                &resource,
                &test_inputs(),
                &test_state(),
                &[],
                &BTreeMap::new(),
                &Cas::new(std::env::temp_dir().join("bit-unused-cas")),
            )
            .unwrap(),
            ReceiptCheck::Unusable
        );
    }

    #[test]
    fn captures_and_restores_image_archive() {
        let dir = tempfile::tempdir().unwrap();
        let docker = dir.path().join("docker");
        let fixture = dir.path().join("fixture.tar");
        let expected = BTreeMap::from([
            ("blobs/sha256/config".to_owned(), b"config".to_vec()),
            ("blobs/sha256/layer".to_owned(), b"shared layer".to_vec()),
            ("manifest.json".to_owned(), b"manifest".to_vec()),
        ]);
        write_archive(
            &fixture,
            &[
                ("manifest.json", b"manifest"),
                ("blobs/sha256/config", b"config"),
                ("blobs/sha256/layer", b"shared layer"),
            ],
        );
        std::fs::write(
            &docker,
            r#"#!/bin/sh
log="$(dirname "$0")/docker.log"
if [ "$1" = "image" ] && [ "$2" = "save" ]; then
  cat "$(dirname "$0")/fixture.tar" > "$4"
  printf 'save %s\n' "$5" >> "$log"
  exit 0
fi
if [ "$1" = "image" ] && [ "$2" = "inspect" ]; then
  exit 1
fi
if [ "$1" = "image" ] && [ "$2" = "load" ]; then
  cat "$4" > "$(dirname "$0")/loaded.tar"
  printf 'load\n' >> "$log"
  exit 0
fi
if [ "$1" = "tag" ]; then
  printf 'tag %s %s\n' "$2" "$3" >> "$log"
  exit 0
fi
exit 1
"#,
        )
        .unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();

        let resource = ImageResource {
            tracker: Arc::new(Mutex::new(FileTracker::default())),
            docker,
        };
        let cas = Cas::new(dir.path().join("cas"));
        let inputs = test_inputs();
        let state = test_state();
        let artifacts = Resource::capture_artifacts(&resource, &inputs, &state, &[], &cas).unwrap();
        assert_eq!(
            artifacts.keys().cloned().collect::<Vec<_>>(),
            expected.keys().cloned().collect::<Vec<_>>()
        );

        let output = crate::output::Output::new(&[]);
        let writer = output.writer("image");
        let restored = Resource::materialize(&resource, &inputs, &state, &[], &artifacts, &cas, &writer)
            .unwrap()
            .unwrap();
        assert_eq!(restored.outputs.image_ref, "myapp:abc123");
        assert_eq!(restored.outputs.image_id, "abc123");
        assert_eq!(restored.state.unwrap().pinned_tag.as_deref(), Some("myapp:abc123"));
        assert_eq!(read_archive(&dir.path().join("loaded.tar")), expected);

        let log = std::fs::read_to_string(dir.path().join("docker.log")).unwrap();
        assert!(log.contains("save myapp:abc123"), "{log}");
        assert!(log.contains("load"), "{log}");
        assert!(log.contains("tag abc123 myapp:abc123"), "{log}");
        assert!(log.contains("tag abc123 myapp:latest"), "{log}");
    }

    #[test]
    fn shared_archive_members_share_cas_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.tar");
        let second = dir.path().join("second.tar");
        write_archive(
            &first,
            &[
                ("manifest.json", b"first manifest"),
                ("blobs/sha256/shared", b"shared layer"),
                ("blobs/sha256/app", b"first app"),
            ],
        );
        write_archive(
            &second,
            &[
                ("manifest.json", b"second manifest"),
                ("blobs/sha256/shared", b"shared layer"),
                ("blobs/sha256/app", b"second app"),
            ],
        );
        let cas = Cas::new(dir.path().join("cas"));

        let first = capture_archive_members(&first, &cas).unwrap();
        let second = capture_archive_members(&second, &cas).unwrap();

        assert_eq!(first["blobs/sha256/shared"], second["blobs/sha256/shared"]);
        assert_ne!(first["blobs/sha256/app"], second["blobs/sha256/app"]);
        assert_ne!(first[MANIFEST_ROLE], second[MANIFEST_ROLE]);
    }

    #[test]
    fn archive_member_paths_must_be_relative_and_normal() {
        assert_eq!(
            archive_member_path("blobs/sha256/layer").unwrap(),
            Path::new("blobs/sha256/layer")
        );
        assert!(archive_member_path("../layer").is_err());
        assert!(archive_member_path("/layer").is_err());
        assert!(archive_member_path("blobs/./layer").is_err());
    }
}
