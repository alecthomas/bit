//! The lockfile `java.maven` writes: what was asked for, and every artifact
//! resolution chose, with its hash and what it depends on.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::provider::BoxError;
use crate::sha256::SHA256;

use super::coords::{Artifact, Key};

/// Changes whenever the format does.
const FORMAT: u32 = 1;

/// What a lockfile was resolved from. A lockfile whose request differs
/// from the target's is out of date.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(in crate::providers::java) struct Request {
    pub artifacts: Vec<String>,
    pub boms: Vec<String>,
    pub repositories: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(in crate::providers::java) struct Lock {
    pub format: u32,
    pub request: Request,
    pub artifacts: BTreeMap<Key, Locked>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(in crate::providers::java) struct Locked {
    pub version: String,
    /// The file's SHA-256. A POM-only artifact has no file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<SHA256>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<Key>,
}

/// A jar a lockfile names.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::providers::java) struct Jar {
    /// Its path in a repository, and in bit's cache.
    pub path: String,
    pub sha256: SHA256,
}

impl Lock {
    pub fn new(request: Request, artifacts: BTreeMap<Key, Locked>) -> Self {
        Self {
            format: FORMAT,
            request,
            artifacts,
        }
    }

    /// The lockfile at `path`, or `None` when there is none yet.
    pub fn read(path: &Path) -> Result<Option<Self>, BoxError> {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display()).into()),
        };
        let lock: Self = serde_json::from_str(&content).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if lock.format != FORMAT {
            return Err(format!(
                "{} has format {}, but this bit reads format {FORMAT}",
                path.display(),
                lock.format
            )
            .into());
        }
        Ok(Some(lock))
    }

    pub fn write(&self, path: &Path) -> Result<(), BoxError> {
        let mut content = serde_json::to_string_pretty(self)?;
        content.push('\n');
        std::fs::write(path, content).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        Ok(())
    }

    /// The jars `keys` need: each key's own and those of everything it
    /// depends on, depth first in declaration order, each once.
    pub fn jars(&self, keys: &[Key]) -> Result<Vec<Jar>, BoxError> {
        let mut seen = Vec::new();
        let mut pending: Vec<&Key> = keys.iter().rev().collect();
        while let Some(key) = pending.pop() {
            if seen.contains(&key) {
                continue;
            }
            let locked = self.artifacts.get(key).ok_or_else(|| {
                format!("`{key}` is not in the lockfile: add it to the `artifacts` of the java.maven target")
            })?;
            seen.push(key);
            pending.extend(locked.dependencies.iter().rev());
        }
        Ok(seen
            .into_iter()
            .filter_map(|key| {
                let locked = &self.artifacts[key];
                let artifact = Artifact {
                    key: key.clone(),
                    version: locked.version.clone(),
                };
                Some(Jar {
                    path: artifact.path(),
                    sha256: locked.sha256?,
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> Key {
        s.parse().unwrap()
    }

    fn lock() -> Lock {
        let locked = |version: &str, sha: Option<&[u8]>, deps: &[&str]| Locked {
            version: version.into(),
            sha256: sha.map(SHA256::digest),
            dependencies: deps.iter().map(|dep| key(dep)).collect(),
        };
        Lock::new(
            Request {
                artifacts: vec!["g:app:1".into()],
                boms: vec![],
                repositories: vec![super::super::repo::CENTRAL.into()],
            },
            BTreeMap::from([
                (key("g:app"), locked("1", Some(b"app"), &["g:a", "g:b"])),
                (key("g:a"), locked("1", Some(b"a"), &["g:shared"])),
                (key("g:b"), locked("2", Some(b"b"), &["g:shared", "g:parent:pom"])),
                (key("g:shared"), locked("3", Some(b"shared"), &[])),
                (key("g:parent:pom"), locked("1", None, &["g:a"])),
            ]),
        )
    }

    #[test]
    fn jars_follow_dependencies_depth_first_once_each() {
        let paths: Vec<String> = lock()
            .jars(&[key("g:app")])
            .unwrap()
            .into_iter()
            .map(|jar| jar.path)
            .collect();
        assert_eq!(
            paths,
            [
                "g/app/1/app-1.jar",
                "g/a/1/a-1.jar",
                "g/shared/3/shared-3.jar",
                "g/b/2/b-2.jar",
            ]
        );
        assert_eq!(lock().jars(&[key("g:shared")]).unwrap().len(), 1);
        let error = lock().jars(&[key("g:missing")]).unwrap_err().to_string();
        assert!(error.contains("`g:missing` is not in the lockfile"), "{error}");
    }

    #[test]
    fn round_trips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("maven.lock.json");
        assert_eq!(Lock::read(&path).unwrap(), None);
        lock().write(&path).unwrap();
        assert_eq!(Lock::read(&path).unwrap(), Some(lock()));
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(json.contains("\"g:parent:pom\": {\n      \"version\": \"1\""), "{json}");
    }
}
