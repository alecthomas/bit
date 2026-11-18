//! Maven dependencies: resolution into a lockfile, and the jars it names.
//!
//! There is no tool to ask for a resolution without Maven itself, so bit
//! follows Maven 3's rules, from Maven Resolver's source.

mod coords;
mod lock;
mod model;
mod pom;
mod repo;
mod resolve;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_tracker::FileTracker;
use crate::output::TargetWriter;
use crate::provider::{
    ApplyResult, BoxError, CachePolicy, Changes, PlanAction, PlanResult, Resource, ResourceKind, Target,
};
use crate::sha256::SHA256;

pub(super) use coords::Key;
use lock::{Lock, Locked, Request};
pub(super) use repo::MavenDirs;
use repo::Repositories;

const DEFAULT_LOCK: &str = "maven.lock.json";

/// Resolve Maven dependencies into a lockfile, with Maven 3's rules: the nearest version of an artifact wins, then the first declared, and requested BOMs manage transitive versions. Resolution reruns only when `artifacts`, `boms` or `repositories` change; otherwise the jars the lockfile names are downloaded and checked against their recorded hashes. Libraries and tests take their jars from the lockfile by `group:artifact`. When JUnit Platform is resolved, its console launcher is added at the same version, for `java.test`. Version ranges and snapshots are not supported.
#[derive(Debug, Deserialize, bit_derive::Schema)]
pub struct JavaMavenInputs {
    /// Artifacts to resolve, as `group:artifact:version`, or `group:artifact[:extension[:classifier]]:version`. The version may be left out when a BOM in `boms` manages it.
    pub artifacts: Vec<String>,
    /// BOMs whose managed versions apply to the artifacts and their dependencies, as `group:artifact:version`
    #[serde(default)]
    pub boms: Vec<String>,
    /// Repository URLs, searched in order (defaults to Maven Central). Mirrors and credentials come from `~/.m2/settings.xml`, whose `<server>` and `<mirrorOf>` ids name Maven Central as `central` and any other repository by its URL.
    #[serde(default)]
    pub repositories: Vec<String>,
    /// Lockfile path, relative to `dir` (defaults to `maven.lock.json`)
    #[serde(default)]
    pub lock: Option<String>,
    /// Fail rather than resolve when the lockfile is out of date, as CI should
    #[serde(default)]
    pub frozen: bool,
}

/// Outputs from a `java.maven` target.
#[derive(Debug, Serialize, bit_derive::Schema)]
pub struct JavaMavenOutputs {
    /// Absolute path to the lockfile, for the `lock` field of libraries and tests
    pub lock: String,
}

/// Persisted state for a `java.maven` target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JavaMavenState {
    pub artifacts: Vec<String>,
    pub boms: Vec<String>,
    pub repositories: Vec<String>,
    pub lock: String,
}

pub struct JavaMavenResource {
    tracker: Arc<FileTracker>,
    dirs: MavenDirs,
}

impl JavaMavenResource {
    pub(super) fn new(tracker: Arc<FileTracker>, dirs: MavenDirs) -> Self {
        Self { tracker, dirs }
    }
}

fn lock_name(inputs: &JavaMavenInputs) -> &str {
    inputs.lock.as_deref().unwrap_or(DEFAULT_LOCK)
}

fn request(inputs: &JavaMavenInputs) -> Request {
    let repositories = if inputs.repositories.is_empty() {
        vec![repo::CENTRAL.to_owned()]
    } else {
        inputs.repositories.clone()
    };
    Request {
        artifacts: inputs.artifacts.clone(),
        boms: inputs.boms.clone(),
        repositories,
    }
}

fn state_for(inputs: &JavaMavenInputs) -> JavaMavenState {
    JavaMavenState {
        artifacts: inputs.artifacts.clone(),
        boms: inputs.boms.clone(),
        repositories: inputs.repositories.clone(),
        lock: lock_name(inputs).to_owned(),
    }
}

/// The JDK version POM profiles' `jdk` activations compare against: the
/// one javac reports where the target runs. Empty when there is none, so
/// that no such profile activates.
fn jdk_version<T>(target: &Target<T>) -> String {
    let dir = target.dir.clone();
    let key = format!("javac --version (in {})", dir.as_deref().unwrap_or("."));
    crate::providers::probe_tool(&key, || {
        let mut cmd = Command::new("javac");
        cmd.arg("--version");
        if let Some(dir) = &dir {
            cmd.current_dir(dir);
        }
        cmd
    })
    .ok()
    .and_then(|output| output.split_whitespace().nth(1).map(str::to_owned))
    .unwrap_or_default()
}

const JUNIT_ENGINE: (&str, &str) = ("org.junit.platform", "junit-platform-engine");
const JUNIT_CONSOLE: (&str, &str) = ("org.junit.platform", "junit-platform-console");

/// The JUnit console launcher, which [`resolve_lock`] adds to a lockfile
/// with JUnit Platform engines.
pub(super) fn junit_console() -> Key {
    Key::new(JUNIT_CONSOLE.0, JUNIT_CONSOLE.1)
}

fn read_lock(path: &Path) -> Result<Lock, BoxError> {
    Lock::read(path)?.ok_or_else(|| {
        format!(
            "{} does not exist: run the java.maven target that writes it",
            path.display()
        )
        .into()
    })
}

/// Whether the lockfile at `path` holds `key`.
pub(super) fn has(path: &Path, key: &Key) -> Result<bool, BoxError> {
    Ok(read_lock(path)?.artifacts.contains_key(key))
}

/// The jars `keys` need from the lockfile at `path`, as paths in the cache
/// with their recorded hashes. Nothing is fetched.
pub(super) fn jars(path: &Path, keys: &[Key], dirs: &MavenDirs) -> Result<Vec<(PathBuf, SHA256)>, BoxError> {
    let jars = read_lock(path)?.jars(keys)?;
    Ok(jars
        .into_iter()
        .map(|jar| (dirs.cache.join(&jar.path), jar.sha256))
        .collect())
}

/// [`jars`], fetched from the lockfile's repositories unless already cached,
/// and checked against their recorded hashes.
pub(super) fn fetch_jars(path: &Path, keys: &[Key], dirs: &MavenDirs) -> Result<Vec<PathBuf>, BoxError> {
    let lock = read_lock(path)?;
    let repos = Repositories::configured(&lock.request.repositories, dirs)?;
    lock.jars(keys)?
        .iter()
        .map(|jar| repos.fetch_verified(&jar.path, &jar.sha256))
        .collect()
}

/// Resolve `request` and record every artifact with its hash, fetching
/// each file.
fn resolve_lock(request: &Request, repos: &Repositories, jdk: String) -> Result<Lock, BoxError> {
    let mut requested = request
        .artifacts
        .iter()
        .map(|artifact| coords::parse_request(artifact))
        .collect::<Result<Vec<_>, _>>()?;
    let boms = request
        .boms
        .iter()
        .map(|bom| match coords::parse_request(bom)? {
            (key, Some(version)) => Ok((key, version)),
            (_, None) => Err(format!("BOM `{bom}` has no version").into()),
        })
        .collect::<Result<Vec<_>, BoxError>>()?;
    let mut models = model::Models::new(repos, jdk);
    let mut graph = resolve::resolve(&mut models, &requested, &boms)?;

    // The console launcher must match the platform version the engines
    // use, so it comes from the same resolution rather than the user.
    let engine = Key::new(JUNIT_ENGINE.0, JUNIT_ENGINE.1);
    let console = junit_console();
    if let Some(engine) = graph.get(&engine)
        && !graph.contains_key(&console)
    {
        requested.push((console, Some(engine.version.clone())));
        graph = resolve::resolve(&mut models, &requested, &boms)?;
    }

    let mut artifacts = BTreeMap::new();
    for (key, node) in graph {
        let sha256 = if key.is_pom() {
            None
        } else {
            let artifact = coords::Artifact {
                key: key.clone(),
                version: node.version.clone(),
            };
            Some(crate::providers::hash_file(&repos.fetch(&artifact.path())?)?)
        };
        artifacts.insert(
            key,
            Locked {
                version: node.version,
                sha256,
                dependencies: node.dependencies,
            },
        );
    }
    Ok(Lock::new(request.clone(), artifacts))
}

impl Resource for JavaMavenResource {
    type State = JavaMavenState;
    type Inputs = JavaMavenInputs;
    type Outputs = JavaMavenOutputs;

    fn name(&self) -> &str {
        "maven"
    }

    fn kind(&self) -> ResourceKind {
        ResourceKind::Build
    }

    fn resolve(&self, inputs: &Target<JavaMavenInputs>) -> Result<BTreeMap<String, SHA256>, BoxError> {
        self.tracker
            .hash_files(&[PathBuf::from(inputs.path(lock_name(inputs)))])
    }

    fn plan(
        &self,
        inputs: &Target<JavaMavenInputs>,
        prior_state: Option<&JavaMavenState>,
    ) -> Result<PlanResult, BoxError> {
        if inputs.artifacts.is_empty() {
            return Err("`artifacts` must name at least one artifact".into());
        }
        let description = format!(
            "resolve {} Maven artifacts → {}",
            inputs.artifacts.len(),
            inputs.path(lock_name(inputs))
        );
        let action = match prior_state {
            None => PlanAction::Create,
            Some(prior) if *prior != state_for(inputs) => PlanAction::Update,
            Some(_) => PlanAction::None,
        };
        Ok(PlanResult {
            action,
            description,
            reason: None,
        })
    }

    fn apply(
        &self,
        inputs: &Target<JavaMavenInputs>,
        _prior_state: Option<&JavaMavenState>,
        _changes: &Changes,
        writer: &TargetWriter,
    ) -> Result<ApplyResult<JavaMavenState, JavaMavenOutputs>, BoxError> {
        let path = PathBuf::from(inputs.path(lock_name(inputs)));
        let request = request(inputs);
        let repos = Repositories::configured(&request.repositories, &self.dirs)?;
        let lock = match Lock::read(&path)? {
            Some(lock) if lock.request == request => lock,
            existing if inputs.frozen => {
                let problem = if existing.is_some() {
                    "is out of date"
                } else {
                    "does not exist"
                };
                return Err(format!("{} {problem}, and `frozen` is set", path.display()).into());
            }
            _ => {
                writer.line(&format!("resolving {} artifacts", request.artifacts.len()));
                let lock = resolve_lock(&request, &repos, jdk_version(inputs))?;
                lock.write(&path)?;
                lock
            }
        };
        // Fetched now so that targets using the jars find them in place.
        for (key, locked) in &lock.artifacts {
            if let Some(sha256) = &locked.sha256 {
                let artifact = coords::Artifact {
                    key: key.clone(),
                    version: locked.version.clone(),
                };
                repos.fetch_verified(&artifact.path(), sha256)?;
            }
        }
        Ok(ApplyResult {
            outputs: JavaMavenOutputs {
                lock: super::absolute(inputs, lock_name(inputs))?
                    .to_string_lossy()
                    .into_owned(),
            },
            state: Some(state_for(inputs)),
        })
    }

    fn destroy(&self, _prior_state: &Target<JavaMavenState>, _writer: &TargetWriter) -> Result<(), BoxError> {
        // The lockfile is checked in, like any other lockfile.
        Ok(())
    }

    /// Resolution can rewrite the lockfile, which is also an input, so the
    /// target's key before and after a run would differ.
    fn cache_policy(&self, _inputs: &Target<JavaMavenInputs>) -> CachePolicy {
        CachePolicy::Local
    }

    fn output_keys(&self, inputs: &Target<JavaMavenInputs>) -> Vec<String> {
        vec![inputs.path(lock_name(inputs))]
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::output::Output;

    /// A file repository holding a small graph: `g:app` depends on `g:lib`,
    /// with jars and SHA-1s for each.
    fn remote() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let put = |path: &str, content: &str| {
            let file = dir.path().join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, content).unwrap();
        };
        put(
            "g/app/1/app-1.pom",
            "<project><groupId>g</groupId><artifactId>app</artifactId><version>1</version>\
             <dependencies><dependency><groupId>g</groupId><artifactId>lib</artifactId><version>2</version></dependency></dependencies></project>",
        );
        put("g/app/1/app-1.jar", "app jar");
        put(
            "g/lib/2/lib-2.pom",
            "<project><groupId>g</groupId><artifactId>lib</artifactId><version>2</version></project>",
        );
        put("g/lib/2/lib-2.jar", "lib jar");
        dir
    }

    fn target(dir: &Path, remote: &Path, frozen: bool) -> Target<JavaMavenInputs> {
        Target::new(
            Some(dir.to_string_lossy().into_owned()),
            JavaMavenInputs {
                artifacts: vec!["g:app:1".into()],
                boms: vec![],
                repositories: vec![format!("file://{}", remote.display())],
                lock: None,
                frozen,
            },
        )
    }

    fn resource(cache: &Path) -> JavaMavenResource {
        let dirs = MavenDirs {
            cache: cache.to_path_buf(),
            user: None,
        };
        JavaMavenResource::new(Arc::new(FileTracker::default()), dirs)
    }

    fn apply(resource: &JavaMavenResource, inputs: &Target<JavaMavenInputs>) -> Result<String, BoxError> {
        let output = Output::new(&[]);
        Ok(
            Resource::apply(resource, inputs, None, &Changes::All, &output.writer("maven"))?
                .outputs
                .lock,
        )
    }

    #[test]
    fn writes_a_lockfile_and_reuses_it_until_the_request_changes() {
        let remote = remote();
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let resource = resource(cache.path());

        let lock_path = apply(&resource, &target(project.path(), remote.path(), false)).unwrap();
        let lock = Lock::read(Path::new(&lock_path)).unwrap().unwrap();
        let app = &lock.artifacts[&Key::new("g", "app")];
        assert_eq!(app.dependencies, [Key::new("g", "lib")]);
        assert_eq!(app.sha256, Some(SHA256::digest(b"app jar")));
        assert_eq!(lock.jars(&[Key::new("g", "app")]).unwrap().len(), 2, "{lock:?}");
        assert!(cache.path().join("g/lib/2/lib-2.jar").is_file());

        // A matching lockfile is used as it is, even with the repository
        // gone, once the jars are cached.
        fs::remove_dir_all(remote.path().join("g/app")).unwrap();
        apply(&resource, &target(project.path(), remote.path(), true)).unwrap();

        let mut changed = target(project.path(), remote.path(), true);
        changed.artifacts.push("g:lib:2".into());
        let error = apply(&resource, &changed).unwrap_err().to_string();
        assert!(error.contains("is out of date, and `frozen` is set"), "{error}");
    }

    #[test]
    fn a_tampered_jar_fails_its_recorded_hash() {
        let remote = remote();
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let resource = resource(cache.path());
        apply(&resource, &target(project.path(), remote.path(), false)).unwrap();

        fs::write(cache.path().join("g/lib/2/lib-2.jar"), "tampered").unwrap();
        fs::write(remote.path().join("g/lib/2/lib-2.jar"), "tampered").unwrap();
        let error = apply(&resource, &target(project.path(), remote.path(), true))
            .unwrap_err()
            .to_string();
        assert!(error.contains("the lockfile records"), "{error}");
    }

    #[test]
    fn junit_engines_bring_the_console_launcher_at_their_version() {
        let dir = tempfile::tempdir().unwrap();
        let put = |path: &str, content: &str| {
            let file = dir.path().join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, content).unwrap();
        };
        let pom = |group: &str, artifact: &str, version: &str, deps: &str| {
            let path = format!("{}/{artifact}/{version}/{artifact}-{version}", group.replace('.', "/"));
            put(
                &format!("{path}.pom"),
                &format!(
                    "<project><groupId>{group}</groupId><artifactId>{artifact}</artifactId><version>{version}</version>{deps}</project>"
                ),
            );
            put(&format!("{path}.jar"), artifact);
        };
        pom(
            "org.junit.jupiter",
            "junit-jupiter-engine",
            "5.12.1",
            "<dependencies><dependency><groupId>org.junit.platform</groupId><artifactId>junit-platform-engine</artifactId><version>1.12.1</version></dependency></dependencies>",
        );
        pom("org.junit.platform", "junit-platform-engine", "1.12.1", "");
        pom("org.junit.platform", "junit-platform-console", "1.12.1", "");

        let cache = tempfile::tempdir().unwrap();
        let request = Request {
            artifacts: vec!["org.junit.jupiter:junit-jupiter-engine:5.12.1".into()],
            boms: vec![],
            repositories: vec![format!("file://{}", dir.path().display())],
        };
        let dirs = MavenDirs {
            cache: cache.path().to_path_buf(),
            user: None,
        };
        let repos = Repositories::configured(&request.repositories, &dirs).unwrap();
        let lock = resolve_lock(&request, &repos, "25".into()).unwrap();
        assert_eq!(
            lock.artifacts[&Key::new("org.junit.platform", "junit-platform-console")].version,
            "1.12.1"
        );
    }
}
