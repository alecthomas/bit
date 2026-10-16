use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{LazyLock, Mutex};

use crate::file_tracker::FileTracker;
use crate::output::{BlockWriter, Event};
use crate::provider::{Ambient, BoxError, VersionCommand};
use crate::sha256::{Hasher, SHA256};

pub mod docker;
pub mod exec;
pub mod go;
pub mod pnpm;
pub mod protobuf;
pub mod rust;

/// How much of a test run to show. Shared by the test resources that report
/// individual tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Deserialize, bit_derive::Schema)]
#[serde(rename_all = "lowercase")]
pub enum ReportMode {
    /// One line per suite.
    #[default]
    Suites,
    /// One line per test.
    Tests,
    /// One line per test, plus the test command's logs as they arrive.
    Verbose,
}

impl ReportMode {
    /// Whether individual tests get their own lines.
    pub fn per_test(self) -> bool {
        self != Self::Suites
    }
}

/// Remove a provider-owned file, directory, or symlink. Missing paths are
/// already clean; every other filesystem error must reach the engine so it
/// does not discard state after an incomplete destroy.
pub(crate) fn remove_path(path: &Path, writer: &BlockWriter) -> Result<(), BoxError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("failed to inspect {}: {error}", path.display()).into()),
    };

    if metadata.is_dir() {
        writer.event(Event::Starting, &format!("rm -rf {}", path.display()));
        fs::remove_dir_all(path).map_err(|error| format!("failed to remove {}: {error}", path.display()))?;
    } else {
        writer.event(Event::Starting, &format!("rm {}", path.display()));
        fs::remove_file(path).map_err(|error| format!("failed to remove {}: {error}", path.display()))?;
    }
    Ok(())
}

/// Symlink hops `hash_file` follows before treating the chain as a cycle.
/// Matches Linux's `MAXSYMLINKS`.
const MAX_SYMLINK_HOPS: usize = 40;

/// Compute a SHA256 hash for an input file.
///
/// A regular file hashes to the digest of its contents. A symlink also hashes
/// every target in its chain, so retargeting a link changes the hash even when
/// the final content does not. Hermit relies on this: every tool's stub chains
/// to the same `bin/hermit` script, and only the link targets name the
/// version. Targets are hashed as stored by readlink, not resolved, so the
/// hash is the same across checkouts.
pub fn hash_file(path: &Path) -> Result<SHA256, BoxError> {
    let chain = LinkChain::of(path)?;
    chain.hash()
}

/// A path's symlink chain: each link target as stored, and the file the
/// chain ends at. Reading it costs one `readlink` per hop, not the content.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct LinkChain {
    pub targets: Vec<PathBuf>,
    pub file: PathBuf,
}

impl LinkChain {
    pub fn of(path: &Path) -> Result<Self, BoxError> {
        let mut file = path.to_path_buf();
        let mut targets = Vec::new();
        while fs::symlink_metadata(&file)?.is_symlink() {
            if targets.len() == MAX_SYMLINK_HOPS {
                return Err(format!("too many levels of symlinks hashing {}", path.display()).into());
            }
            let target = fs::read_link(&file)?;
            // A relative target is relative to the link's directory; joining
            // an absolute target replaces the path.
            file = file.parent().unwrap_or(Path::new("")).join(&target);
            targets.push(target);
        }
        Ok(Self { targets, file })
    }

    /// The [`hash_file`] digest: the link targets, then the file's content.
    pub fn hash(&self) -> Result<SHA256, BoxError> {
        let mut hasher = Hasher::new();
        for target in &self.targets {
            hasher.update(target.as_os_str().as_encoded_bytes());
            hasher.update(b"\0");
        }
        // Streamed, so a large executable is never held in memory whole.
        let mut file = fs::File::open(&self.file)?;
        let mut buffer = vec![0; 64 * 1024];
        loop {
            let read = std::io::Read::read(&mut file, &mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(hasher.finalize())
    }
}

/// Locate `program` as a command run from bit's working directory would: a
/// name containing `/` is a path, and any other name is searched for on
/// `PATH`.
pub fn find_executable(program: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    if program.contains('/') {
        let path = PathBuf::from(program);
        return path.is_file().then_some(path);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|entry| entry.join(program))
        .find(|path| {
            fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        })
}

/// Fingerprint the ambient state a resource declares, as sources. See
/// [`crate::provider::Resource::ambient`].
///
/// An executable found on disk is keyed by its path, so a Git change to it,
/// such as a Hermit upgrade, maps to the blocks that run it. Every other
/// fingerprint is keyed by an [`AMBIENT_KEY_PREFIXES`] prefix, which no path
/// has.
///
/// A tool that cannot be found or run is recorded as such rather than
/// failing the block: running it fails anyway, and recording it means
/// installing it later reruns the block. An unset variable is omitted, so
/// setting it later is a new source.
pub fn hash_ambient(ambient: &[Ambient], tracker: &mut FileTracker) -> Result<BTreeMap<String, SHA256>, BoxError> {
    let mut hashes = BTreeMap::new();
    for item in ambient {
        match item {
            Ambient::Version(command) => {
                let key = format!("version:{}", command.display());
                let output = probe_tool(&command.probe_key(), || command.command());
                let hash = match output {
                    Ok(output) => SHA256::digest(output.as_bytes()),
                    Err(_) => SHA256::digest(b"unavailable"),
                };
                hashes.insert(key, hash);
            }
            Ambient::Executable(name) => match find_executable(name) {
                Some(path) => {
                    hashes.insert(path.to_string_lossy().into_owned(), tracker.hash_executable(&path)?);
                }
                None => {
                    hashes.insert(format!("executable:{name}"), SHA256::digest(b"missing"));
                }
            },
            Ambient::Variable(name) => {
                if let Some(value) = std::env::var_os(name) {
                    hashes.insert(format!("env:{name}"), SHA256::digest(value.as_encoded_bytes()));
                }
            }
        }
    }
    Ok(hashes)
}

/// Prefixes of [`hash_ambient`] keys that are not paths. Git cannot report a
/// change to them, so change detection compares them directly.
pub const AMBIENT_KEY_PREFIXES: [&str; 3] = ["version:", "executable:", "env:"];

/// A tool's version command, as an [`Ambient::Version`].
pub(crate) fn version(program: &str, args: &[&str], dir: Option<&str>) -> Ambient {
    VersionCommand::new(program, args).dir(dir).ambient()
}

/// Per-process cache of tool probe output (versions, `go env`, ...) so a run
/// with many blocks pays for each probe once.
static PROBE_CACHE: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Run `build()` once per distinct `key` and return its trimmed stdout.
pub fn probe_tool(key: &str, build: impl FnOnce() -> Command) -> Result<String, BoxError> {
    if let Some(cached) = PROBE_CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(key) {
        return Ok(cached.clone());
    }
    let mut cmd = build();
    let program = cmd.get_program().to_string_lossy().into_owned();
    let output = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("failed to run `{program}`: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("`{program}` exited with {}: {}", output.status, stderr.trim()).into());
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    PROBE_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.to_owned(), text.clone());
    Ok(text)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::output::Output;

    #[test]
    fn remove_path_removes_files_and_directories_and_allows_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        let child_dir = dir.path().join("dir");
        fs::write(&file, "content").unwrap();
        fs::create_dir(&child_dir).unwrap();
        fs::write(child_dir.join("child"), "content").unwrap();
        let output = Output::new(&[]);
        let writer = output.writer("test");

        remove_path(&file, &writer).unwrap();
        remove_path(&child_dir, &writer).unwrap();
        remove_path(&dir.path().join("missing"), &writer).unwrap();

        assert!(!file.exists());
        assert!(!child_dir.exists());
    }

    #[test]
    fn remove_path_reports_inspection_errors() {
        let output = Output::new(&[]);
        let error = remove_path(Path::new("invalid\0path"), &output.writer("test")).unwrap_err();
        assert!(error.to_string().contains("failed to inspect"));
    }

    /// Lay out a Hermit-style chain: `tsk -> .tsk-<version>.pkg -> hermit`.
    fn hermit_chain(dir: &Path, version: &str) -> std::path::PathBuf {
        let pkg = format!(".tsk-{version}.pkg");
        let _ = fs::remove_file(dir.join("tsk"));
        let _ = fs::remove_file(dir.join(&pkg));
        symlink("hermit", dir.join(&pkg)).unwrap();
        symlink(&pkg, dir.join("tsk")).unwrap();
        dir.join("tsk")
    }

    #[test]
    fn hash_file_of_regular_file_is_content_digest() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, "content").unwrap();
        assert_eq!(hash_file(&file).unwrap(), SHA256::digest(b"content"));
    }

    #[test]
    fn hash_file_changes_when_intermediate_link_target_changes() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("hermit"), "script").unwrap();
        let before = hash_file(&hermit_chain(dir.path(), "0.12.1")).unwrap();
        let after = hash_file(&hermit_chain(dir.path(), "0.17.0")).unwrap();
        assert_ne!(before, after);
        assert_ne!(after, SHA256::digest(b"script"));
    }

    #[test]
    fn hash_file_changes_when_content_behind_chain_changes() {
        let dir = tempfile::tempdir().unwrap();
        let tsk = hermit_chain(dir.path(), "0.17.0");
        fs::write(dir.path().join("hermit"), "v1").unwrap();
        let before = hash_file(&tsk).unwrap();
        fs::write(dir.path().join("hermit"), "v2").unwrap();
        assert_ne!(before, hash_file(&tsk).unwrap());
    }

    #[test]
    fn hash_file_uses_relative_link_text_independent_of_location() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        for dir in [a.path(), b.path()] {
            fs::create_dir(dir.join("bin")).unwrap();
            fs::write(dir.join("hermit"), "script").unwrap();
            symlink("../hermit", dir.join("bin/tsk")).unwrap();
        }
        assert_eq!(
            hash_file(&a.path().join("bin/tsk")).unwrap(),
            hash_file(&b.path().join("bin/tsk")).unwrap()
        );
    }

    #[test]
    fn hash_file_rejects_symlink_cycle() {
        let dir = tempfile::tempdir().unwrap();
        symlink("b", dir.path().join("a")).unwrap();
        symlink("a", dir.path().join("b")).unwrap();
        let error = hash_file(&dir.path().join("a")).unwrap_err();
        assert!(error.to_string().contains("too many levels of symlinks"));
    }

    #[test]
    fn find_tool_searches_path_for_names_and_checks_paths() {
        assert!(find_executable("sh").is_some_and(|path| path.ends_with("sh")));
        assert_eq!(find_executable("bit-test-no-such-tool"), None);

        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("tool");
        fs::write(&tool, "#!/bin/sh\n").unwrap();
        let tool = tool.to_string_lossy().into_owned();
        assert_eq!(find_executable(&tool), Some(PathBuf::from(&tool)));
        assert_eq!(find_executable(&format!("{tool}-missing")), None);
    }

    #[test]
    fn hash_tools_fingerprints_each_kind() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("tool");
        fs::write(&tool, "v1").unwrap();
        let tool = tool.to_string_lossy().into_owned();
        let tools = [
            Ambient::Executable(tool.clone()),
            Ambient::Executable("bit-test-no-such-tool".into()),
            VersionCommand::new("echo", &["1.2.3"]).env("A", "b").ambient(),
            version("bit-test-no-such-tool", &["--version"], Some("sub")),
            Ambient::Variable("PATH".into()),
            Ambient::Variable("BIT_TEST_UNSET_VARIABLE".into()),
        ];
        let hashes = hash_ambient(&tools, &mut FileTracker::default()).unwrap();
        let path = std::env::var_os("PATH").unwrap();
        assert_eq!(
            hashes,
            BTreeMap::from([
                (tool, SHA256::digest(b"v1")),
                ("executable:bit-test-no-such-tool".into(), SHA256::digest(b"missing")),
                ("version:A=b echo 1.2.3".into(), SHA256::digest(b"1.2.3")),
                (
                    "version:bit-test-no-such-tool --version".into(),
                    SHA256::digest(b"unavailable")
                ),
                ("env:PATH".into(), SHA256::digest(path.as_encoded_bytes())),
            ])
        );
    }
}
