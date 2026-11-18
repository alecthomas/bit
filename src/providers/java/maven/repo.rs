//! Fetching files from Maven repositories.
//!
//! Every file is downloaded once into bit's cache, laid out as in a
//! repository, and read from there afterwards. A file already in the local
//! Maven repository is copied from it instead. Maven Central limits how much
//! clients download and asks tools to cache and back off, so nothing is
//! fetched that the cache holds, and a 429 or server error is retried after
//! a delay.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use sha1::Digest;

use crate::provider::BoxError;
use crate::sha256::SHA256;

pub(in crate::providers::java) const CENTRAL: &str = "https://repo.maven.apache.org/maven2";

/// Attempts at a download that a 429 or server error interrupts.
const ATTEMPTS: u32 = 4;

/// Where Maven files live on this machine.
#[derive(Debug, Clone)]
pub(in crate::providers::java) struct MavenDirs {
    /// bit's cache of fetched files.
    pub cache: PathBuf,
    /// Maven's user directory, `~/.m2`, with `settings.xml` and the local
    /// repository. `None` reads neither.
    pub user: Option<PathBuf>,
}

impl MavenDirs {
    pub fn new() -> Self {
        let cache = crate::cache::default_dir().unwrap_or_else(|_| std::env::temp_dir().join("bit"));
        Self {
            cache: cache.join("maven"),
            user: dirs::home_dir().map(|home| home.join(".m2")),
        }
    }
}

/// Repositories to fetch from, in order, and where fetched files go.
pub(in crate::providers::java) struct Repositories {
    remotes: Vec<Remote>,
    cache: PathBuf,
    local: Option<PathBuf>,
    agent: ureq::Agent,
}

struct Remote {
    /// The URL as configured, which names the repository in errors.
    url: String,
    /// Where files are fetched from: `url`, or the mirror that replaces it.
    fetch: String,
    authorization: Option<String>,
}

impl Repositories {
    /// `urls` as Maven's `settings.xml` configures them, with mirrors and
    /// credentials applied.
    pub fn configured(urls: &[String], dirs: &MavenDirs) -> Result<Self, BoxError> {
        let settings = match dirs.user.as_ref().map(|user| user.join("settings.xml")) {
            Some(path) if path.is_file() => Settings::parse(&fs::read_to_string(&path)?)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?,
            _ => Settings::default(),
        };
        let local = dirs.user.as_ref().map(|user| user.join("repository"));
        Self::new(urls, dirs.cache.clone(), local, &settings)
    }

    fn new(urls: &[String], cache: PathBuf, local: Option<PathBuf>, settings: &Settings) -> Result<Self, BoxError> {
        let remotes = urls
            .iter()
            .map(|url| {
                let url = url.trim_end_matches('/').to_owned();
                let id = repository_id(&url);
                let (server, fetch) = match settings.mirror_for(&id, &url) {
                    Some(mirror) => (mirror.id.clone(), mirror.url.trim_end_matches('/').to_owned()),
                    None => (id, url.clone()),
                };
                let authorization = settings.authorization(&server)?;
                Ok(Remote {
                    url,
                    fetch,
                    authorization,
                })
            })
            .collect::<Result<_, BoxError>>()?;
        let local = settings.local_repository.clone().or(local);
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(300)))
            .user_agent("bit")
            .build()
            .into();
        Ok(Self {
            remotes,
            cache,
            local,
            agent,
        })
    }

    /// Where `path` is kept once fetched.
    pub fn cached(&self, path: &str) -> PathBuf {
        self.cache.join(path)
    }

    /// The file at `path` in the first repository that has it, fetched
    /// unless already cached.
    pub fn fetch(&self, path: &str) -> Result<PathBuf, BoxError> {
        let cached = self.cached(path);
        if cached.is_file() {
            return Ok(cached);
        }
        if self.copy_local(path, &cached, None)? {
            return Ok(cached);
        }
        for remote in &self.remotes {
            if self.download(remote, path, &cached)? {
                return Ok(cached);
            }
        }
        let searched: Vec<&str> = self.remotes.iter().map(|remote| remote.url.as_str()).collect();
        Err(format!("{path} is in none of the repositories {}", searched.join(", ")).into())
    }

    /// The file at `path`, fetched unless already cached, which must have
    /// the SHA-256 `expected`.
    pub fn fetch_verified(&self, path: &str, expected: &SHA256) -> Result<PathBuf, BoxError> {
        let cached = self.cached(path);
        if !cached.is_file() && !self.copy_local(path, &cached, Some(expected))? {
            self.fetch(path)?;
        }
        let actual = crate::providers::hash_file(&cached)?;
        if actual != *expected {
            fs::remove_file(&cached)?;
            return Err(format!("{path} has SHA-256 {actual}, but the lockfile records {expected}").into());
        }
        Ok(cached)
    }

    /// Copy `path` from the local Maven repository, if it has it with the
    /// expected content.
    fn copy_local(&self, path: &str, dest: &Path, expected: Option<&SHA256>) -> Result<bool, BoxError> {
        let Some(local) = self
            .local
            .as_ref()
            .map(|local| local.join(path))
            .filter(|local| local.is_file())
        else {
            return Ok(false);
        };
        let staged = staging(dest)?;
        fs::copy(&local, staged.path())?;
        fs::set_permissions(staged.path(), std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
        if expected.is_some_and(|expected| crate::providers::hash_file(staged.path()).ok().as_ref() != Some(expected)) {
            return Ok(false);
        }
        persist(staged, dest)?;
        Ok(true)
    }

    /// Download `path` from `remote` to `dest`, checking it against the
    /// repository's SHA-1 when there is one. `false` when the repository
    /// doesn't have it.
    fn download(&self, remote: &Remote, path: &str, dest: &Path) -> Result<bool, BoxError> {
        let url = format!("{}/{path}", remote.fetch);
        let staged = staging(dest)?;
        if let Some(local) = url.strip_prefix("file://") {
            if !Path::new(local).is_file() {
                return Ok(false);
            }
            fs::copy(local, staged.path())?;
        } else {
            let Some(mut body) = self.get(remote, &url)? else {
                return Ok(false);
            };
            std::io::copy(&mut body, &mut fs::File::create(staged.path())?)
                .map_err(|e| format!("failed to download {url}: {e}"))?;
        }
        if let Some(expected) = self.sha1(remote, &format!("{url}.sha1"))? {
            let actual = hex(&sha1::Sha1::digest(fs::read(staged.path())?));
            if actual != expected {
                return Err(format!("{url} has SHA-1 {actual}, but the repository says {expected}").into());
            }
        }
        persist(staged, dest)?;
        Ok(true)
    }

    /// The SHA-1 a repository publishes beside a file: the first word of
    /// the `.sha1` file, as some tools append the file name.
    fn sha1(&self, remote: &Remote, url: &str) -> Result<Option<String>, BoxError> {
        let content = match url.strip_prefix("file://") {
            Some(local) => fs::read_to_string(local).ok(),
            None => match self.get(remote, url)? {
                Some(mut body) => {
                    let mut content = String::new();
                    body.read_to_string(&mut content)?;
                    Some(content)
                }
                None => None,
            },
        };
        Ok(content.and_then(|content| content.split_whitespace().next().map(str::to_ascii_lowercase)))
    }

    /// GET `url`, retrying 429s and server errors with backoff. `None` for
    /// a 404.
    fn get(&self, remote: &Remote, url: &str) -> Result<Option<impl Read + use<>>, BoxError> {
        let mut delay = Duration::from_secs(1);
        for attempt in 1..=ATTEMPTS {
            let mut request = self.agent.get(url);
            if let Some(authorization) = &remote.authorization {
                request = request.header("Authorization", authorization);
            }
            let response = match request.call() {
                Ok(response) => response,
                Err(e) if attempt < ATTEMPTS => {
                    crate::output::debug_current(|| format!("GET {url}: {e}; retrying"));
                    std::thread::sleep(delay);
                    delay *= 2;
                    continue;
                }
                Err(e) => return Err(format!("failed to fetch {url}: {e}").into()),
            };
            let status = response.status().as_u16();
            match status {
                200 => return Ok(Some(response.into_body().into_reader())),
                404 | 410 => return Ok(None),
                429 | 500..=599 if attempt < ATTEMPTS => {
                    let wait = response
                        .headers()
                        .get("retry-after")
                        .and_then(|value| value.to_str().ok()?.parse().ok())
                        .map(Duration::from_secs)
                        .unwrap_or(delay);
                    crate::output::debug_current(|| format!("GET {url}: {status}; retrying in {wait:?}"));
                    std::thread::sleep(wait);
                    delay *= 2;
                }
                401 | 403 => {
                    return Err(format!(
                        "{url} refused access ({status}); credentials come from a <server> in ~/.m2/settings.xml"
                    )
                    .into());
                }
                _ => return Err(format!("failed to fetch {url}: HTTP {status}").into()),
            }
        }
        Err(format!("failed to fetch {url}: still failing after {ATTEMPTS} attempts").into())
    }
}

/// A temporary file beside `dest`, so that it can be renamed into place
/// with [`persist`].
fn staging(dest: &Path) -> Result<tempfile::NamedTempFile, BoxError> {
    let dir = dest.parent().ok_or("cache path has no directory")?;
    fs::create_dir_all(dir)?;
    Ok(tempfile::NamedTempFile::new_in(dir)?)
}

/// Move a staged file into place. Cached files are made readable and not
/// executable whatever their source's mode, because [`hash_file`] hashes
/// the executable bit, and a recorded hash must match on every machine.
///
/// [`hash_file`]: crate::providers::hash_file
fn persist(staged: tempfile::NamedTempFile, dest: &Path) -> Result<(), BoxError> {
    fs::set_permissions(staged.path(), std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
    staged.persist(dest)?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The id settings use for a repository: `central` for Maven Central, as
/// in Maven, and otherwise its URL.
fn repository_id(url: &str) -> String {
    match url {
        CENTRAL | "https://repo1.maven.org/maven2" => "central".to_owned(),
        _ => url.to_owned(),
    }
}

/// What dependency resolution reads from Maven's `settings.xml`.
#[derive(Debug, Default)]
struct Settings {
    local_repository: Option<PathBuf>,
    mirrors: Vec<Mirror>,
    servers: Vec<Server>,
}

#[derive(Debug)]
struct Mirror {
    id: String,
    url: String,
    mirror_of: String,
}

#[derive(Debug)]
struct Server {
    id: String,
    username: Option<String>,
    password: Option<String>,
}

impl Settings {
    fn parse(xml: &str) -> Result<Self, BoxError> {
        let document = roxmltree::Document::parse(xml)?;
        let root = document.root_element();
        let text = |node: roxmltree::Node, name: &str| {
            node.children()
                .find(|child| child.tag_name().name() == name)
                .map(|child| interpolate(child.text().unwrap_or("").trim()))
        };
        let elements = |name: &'static str, item: &'static str| {
            root.children()
                .filter(move |child| child.tag_name().name() == name)
                .flat_map(move |list| list.children().filter(move |child| child.tag_name().name() == item))
        };
        Ok(Self {
            local_repository: text(root, "localRepository").map(PathBuf::from),
            mirrors: elements("mirrors", "mirror")
                .filter_map(|mirror| {
                    Some(Mirror {
                        id: text(mirror, "id").unwrap_or_default(),
                        url: text(mirror, "url")?,
                        mirror_of: text(mirror, "mirrorOf")?,
                    })
                })
                .collect(),
            servers: elements("servers", "server")
                .filter_map(|server| {
                    Some(Server {
                        id: text(server, "id")?,
                        username: text(server, "username"),
                        password: text(server, "password"),
                    })
                })
                .collect(),
        })
    }

    /// The mirror for repository `id` at `url`, as Maven's mirror selector
    /// picks it: one naming the id exactly, then the first whose
    /// `mirrorOf` pattern matches.
    fn mirror_for(&self, id: &str, url: &str) -> Option<&Mirror> {
        self.mirrors.iter().find(|mirror| mirror.mirror_of == id).or_else(|| {
            self.mirrors
                .iter()
                .find(|mirror| mirror_matches(&mirror.mirror_of, id, url))
        })
    }

    fn authorization(&self, id: &str) -> Result<Option<String>, BoxError> {
        let Some(server) = self.servers.iter().find(|server| server.id == id) else {
            return Ok(None);
        };
        let (Some(username), Some(password)) = (&server.username, &server.password) else {
            return Ok(None);
        };
        if password.starts_with('{') && password.ends_with('}') {
            return Err(
                format!("the password for server `{id}` in settings.xml is encrypted, which bit can't read").into(),
            );
        }
        let credentials = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        Ok(Some(format!("Basic {credentials}")))
    }
}

/// Whether a `mirrorOf` pattern covers repository `id`: a comma-separated
/// list of ids, `*`, `external:*` for anything not on this machine, and
/// `!id` to exclude one.
fn mirror_matches(pattern: &str, id: &str, url: &str) -> bool {
    let mut matched = false;
    for part in pattern.split(',').map(str::trim) {
        if let Some(excluded) = part.strip_prefix('!') {
            if excluded == id {
                return false;
            }
        } else if part == id || part == "*" || (part == "external:*" && is_external(url)) {
            matched = true;
        }
    }
    matched
}

fn is_external(url: &str) -> bool {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or("")
        .split(['/', ':'])
        .next()
        .unwrap_or("");
    !url.starts_with("file:") && host != "localhost" && host != "127.0.0.1"
}

/// Replace `${env.NAME}` and `${user.home}` in a settings value, as Maven
/// does. Unknown expressions are left as written.
fn interpolate(value: &str) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let expression = &rest[start + 2..start + end];
        let replacement = match expression.strip_prefix("env.") {
            Some(name) => std::env::var(name).ok(),
            None if expression == "user.home" => dirs::home_dir().map(|home| home.to_string_lossy().into_owned()),
            None => None,
        };
        out.push_str(&replacement.unwrap_or_else(|| rest[start..=start + end].to_owned()));
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository on disk holding `files`, with SHA-1 files beside them.
    fn repository(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, content) in files {
            let file = dir.path().join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, content).unwrap();
            fs::write(format!("{}.sha1", file.display()), hex(&sha1::Sha1::digest(content))).unwrap();
        }
        dir
    }

    fn repositories(remotes: &[&Path], cache: &Path) -> Repositories {
        let urls: Vec<String> = remotes.iter().map(|dir| format!("file://{}", dir.display())).collect();
        Repositories::new(&urls, cache.to_path_buf(), None, &Settings::default()).unwrap()
    }

    #[test]
    fn fetches_from_the_first_repository_that_has_the_file() {
        let first = repository(&[("a/1/a-1.pom", "first")]);
        let second = repository(&[("b/1/b-1.pom", "second"), ("a/1/a-1.pom", "shadowed")]);
        let cache = tempfile::tempdir().unwrap();
        let repos = repositories(&[first.path(), second.path()], cache.path());

        assert_eq!(
            fs::read_to_string(repos.fetch("a/1/a-1.pom").unwrap()).unwrap(),
            "first"
        );
        assert_eq!(
            fs::read_to_string(repos.fetch("b/1/b-1.pom").unwrap()).unwrap(),
            "second"
        );
        let error = repos.fetch("c/1/c-1.pom").unwrap_err().to_string();
        assert!(error.contains("is in none of the repositories"), "{error}");
    }

    #[test]
    fn cached_files_are_not_fetched_again() {
        let remote = repository(&[("a/1/a-1.jar", "jar")]);
        let cache = tempfile::tempdir().unwrap();
        let repos = repositories(&[remote.path()], cache.path());
        repos.fetch("a/1/a-1.jar").unwrap();
        fs::remove_dir_all(remote.path().join("a")).unwrap();
        assert_eq!(fs::read_to_string(repos.fetch("a/1/a-1.jar").unwrap()).unwrap(), "jar");
    }

    #[test]
    fn a_checksum_mismatch_is_an_error() {
        let remote = repository(&[("a/1/a-1.jar", "jar")]);
        fs::write(remote.path().join("a/1/a-1.jar.sha1"), "0000  a-1.jar").unwrap();
        let cache = tempfile::tempdir().unwrap();
        let error = repositories(&[remote.path()], cache.path())
            .fetch("a/1/a-1.jar")
            .unwrap_err()
            .to_string();
        assert!(error.contains("has SHA-1"), "{error}");
        assert!(!cache.path().join("a/1/a-1.jar").exists());
    }

    #[test]
    fn verified_fetches_check_the_recorded_hash() {
        let remote = repository(&[("a/1/a-1.jar", "jar")]);
        let cache = tempfile::tempdir().unwrap();
        let repos = repositories(&[remote.path()], cache.path());
        repos.fetch_verified("a/1/a-1.jar", &SHA256::digest(b"jar")).unwrap();
        let error = repos
            .fetch_verified("a/1/a-1.jar", &SHA256::digest(b"other"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("the lockfile records"), "{error}");
    }

    #[test]
    fn local_repository_files_are_copied_when_they_match() {
        let local = repository(&[("a/1/a-1.jar", "local")]);
        let cache = tempfile::tempdir().unwrap();
        let mut repos = repositories(&[], cache.path());
        repos.local = Some(local.path().to_path_buf());
        let error = repos
            .fetch_verified("a/1/a-1.jar", &SHA256::digest(b"other"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("the lockfile records"), "{error}");
        assert!(!cache.path().join("a/1/a-1.jar").exists());
        let path = repos.fetch_verified("a/1/a-1.jar", &SHA256::digest(b"local")).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "local");
    }

    const SETTINGS: &str = r#"<settings>
  <localRepository>/tmp/m2</localRepository>
  <mirrors>
    <mirror><id>corp</id><url>https://nexus.example.com/maven/</url><mirrorOf>external:*,!internal</mirrorOf></mirror>
    <mirror><id>exact</id><url>https://exact.example.com</url><mirrorOf>special</mirrorOf></mirror>
  </mirrors>
  <servers>
    <server><id>corp</id><username>me</username><password>${env.BIT_TEST_MAVEN_PASSWORD}</password></server>
    <server><id>secret</id><username>me</username><password>{COQLCE6DU6GtcS5P=}</password></server>
  </servers>
</settings>"#;

    #[test]
    fn settings_mirror_repositories_with_credentials() {
        let settings = Settings::parse(SETTINGS).unwrap();
        assert_eq!(settings.local_repository, Some(PathBuf::from("/tmp/m2")));
        assert_eq!(settings.mirror_for("central", CENTRAL).unwrap().id, "corp");
        assert_eq!(
            settings.mirror_for("special", "https://x.example.com").unwrap().id,
            "exact"
        );
        assert!(settings.mirror_for("internal", "https://i.example.com").is_none());
        assert!(
            settings
                .mirror_for("http://localhost:8080/repo", "http://localhost:8080/repo")
                .is_none()
        );

        let repos = Repositories::new(&[CENTRAL.to_owned()], PathBuf::from("/c"), None, &settings).unwrap();
        assert_eq!(repos.remotes[0].fetch, "https://nexus.example.com/maven");
        assert_eq!(repos.remotes[0].url, CENTRAL);
        assert!(repos.remotes[0].authorization.as_deref().unwrap().starts_with("Basic "));

        let error = settings.authorization("secret").unwrap_err().to_string();
        assert!(error.contains("encrypted"), "{error}");
    }

    #[test]
    fn interpolate_replaces_known_expressions_only() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(interpolate("${user.home}/.m2"), format!("{}/.m2", home.display()));
        assert_eq!(interpolate("${unknown} and ${"), "${unknown} and ${");
    }
}
