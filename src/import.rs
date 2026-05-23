use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ast::{Module, Statement};

/// Lock file living next to `BUILD.bit`, pinning each imported git repo to a
/// resolved commit SHA for reproducible builds.
pub const LOCK_FILENAME: &str = "BUILD.bit.lock";

/// Subdirectory under the bit cache root for bare git clones used as a
/// fetch source.
const GIT_CACHE_SUBDIR: &str = "git";

/// Subdirectory under the bit cache root for extracted, immutable per-commit
/// module trees.
const MOD_CACHE_SUBDIR: &str = "modules";

/// Errors raised while resolving imports.
#[derive(Debug, Error)]
pub enum ImportError {
    #[error("invalid import URL '{url}': {hint}")]
    InvalidUrl { url: String, hint: &'static str },
    #[error("invalid local import path '{path}': cannot derive a provider name")]
    InvalidProvider { path: String },
    #[error("local import '{path}' does not exist or is not a directory")]
    LocalImportMissing { path: String },
    #[error("could not locate a git repository in any prefix of '{url}' (probed via `git ls-remote`)")]
    NoRepoFound { url: String },
    #[error(
        "import path '{path}' is given two different provider names: '{first}' and '{second}' (use a single `as` alias)"
    )]
    AliasMismatch {
        path: String,
        first: String,
        second: String,
    },
    #[error("provider '{provider}' is provided by both '{first}' and '{second}'")]
    ProviderConflict {
        provider: String,
        first: String,
        second: String,
    },
    #[error(
        "provider '{provider}' resolved to conflicting commits: {first_sha} (via {first_source}) and {second_sha} (via {second_source})"
    )]
    ShaConflict {
        provider: String,
        first_sha: String,
        first_source: String,
        second_sha: String,
        second_source: String,
    },
    #[error(
        "imported project at {project} is missing a lock entry for '{repo}' (expected in {project}/BUILD.bit.lock)"
    )]
    MissingLockEntry { project: String, repo: String },
    #[error("failed to read {path}: {source}")]
    ChildRead {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse {path}: {message}")]
    ChildParse { path: String, message: String },
    #[error("git clone of {repo_url} failed: {message}")]
    GitClone { repo_url: String, message: String },
    #[error("git fetch in {repo_url} failed: {message}")]
    GitFetch { repo_url: String, message: String },
    #[error("default-branch lookup for {repo_url} failed: {message}")]
    GitDefaultBranch { repo_url: String, message: String },
    #[error("git archive {sha} from {repo_url} failed: {message}")]
    GitArchive {
        repo_url: String,
        sha: String,
        message: String,
    },
    #[error("cannot determine cache directory")]
    NoCacheDir,
    #[error("failed to read {0}: {1}")]
    LockRead(PathBuf, io::Error),
    #[error("failed to parse {0}: {1}")]
    LockParse(PathBuf, toml::de::Error),
    #[error("failed to write {0}: {1}")]
    LockWrite(PathBuf, io::Error),
    #[error("failed to serialize lock file: {0}")]
    LockSerialize(toml::ser::Error),
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// A parsed import URL, validated for grammar but not yet split into repo
/// and subpath (that requires either hardcoded-forge knowledge or probing).
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedImport {
    /// The original URL, normalized (trailing `/` and `.git` stripped).
    pub url: String,
    /// Path segments — host is `[0]`, the rest are path components.
    pub segments: Vec<String>,
}

/// Hosts whose repo boundary is known statically (first 3 path segments).
const KNOWN_FORGES: &[&str] = &["github.com", "gitlab.com", "bitbucket.org"];

/// A git import after its repo / subpath boundary has been discovered.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedRepo {
    /// `<host>/<owner>/<repo>` — lock + cache key. Subpath is excluded.
    pub cache_key: String,
    /// URL passed to `git clone`: always `https://<cache_key>`. For SSH or
    /// auth rewrites, configure `insteadOf` rules in `~/.gitconfig`.
    pub repo_url: String,
    /// Subdirectory within the repo, or `None` for repo-root imports.
    pub subpath: Option<String>,
}

/// Parse an import URL. Accepts only bare `host/path` form (à la Go module
/// paths). Schemes, SSH shorthand, absolute paths, and other shapes are
/// rejected because they don't fit the grammar — not by explicit denylist.
pub fn parse_import_url(url: &str) -> Result<ParsedImport, ImportError> {
    if !is_valid_import_url(url) {
        return Err(ImportError::InvalidUrl {
            url: url.to_owned(),
            hint: "expected a bare `host/path` like `github.com/foo/bar`",
        });
    }
    let trimmed = url.trim_end_matches('/');
    let normalized = trimmed.strip_suffix(".git").unwrap_or(trimmed).trim_end_matches('/');
    Ok(ParsedImport {
        url: normalized.to_owned(),
        segments: normalized.split('/').map(String::from).collect(),
    })
}

/// Probe results cached per-run so the same URL never round-trips twice.
#[derive(Debug, Default)]
struct ProbeCache(BTreeMap<String, ResolvedRepo>);

/// Determine the repo boundary for a parsed URL. Hardcoded forges resolve
/// statically; other hosts are probed via `git ls-remote` from the longest
/// prefix down to the first that responds.
fn resolve_repo_boundary(parsed: &ParsedImport, cache: &mut ProbeCache) -> Result<ResolvedRepo, ImportError> {
    let host = parsed.segments.first().expect("validated URL has a host");
    if KNOWN_FORGES.contains(&host.as_str()) {
        return forge_boundary(parsed);
    }
    if let Some(hit) = cache.0.get(&parsed.url) {
        return Ok(hit.clone());
    }
    let probed = probe_boundary(parsed)?;
    cache.0.insert(parsed.url.clone(), probed.clone());
    Ok(probed)
}

/// Static boundary for a hardcoded forge: first 3 segments = repo.
fn forge_boundary(parsed: &ParsedImport) -> Result<ResolvedRepo, ImportError> {
    if parsed.segments.len() < 3 {
        return Err(ImportError::InvalidUrl {
            url: parsed.url.clone(),
            hint: "known forges require <host>/<owner>/<repo>",
        });
    }
    let cache_key = parsed.segments[..3].join("/");
    let subpath = if parsed.segments.len() > 3 {
        Some(parsed.segments[3..].join("/"))
    } else {
        None
    };
    Ok(ResolvedRepo {
        repo_url: format!("https://{cache_key}"),
        cache_key,
        subpath,
    })
}

/// Probe `git ls-remote https://<prefix>` from longest to shortest. First
/// hit is the repo; remaining segments are the subpath.
fn probe_boundary(parsed: &ParsedImport) -> Result<ResolvedRepo, ImportError> {
    // Need at least host + one segment to have a candidate repo URL.
    for i in (2..=parsed.segments.len()).rev() {
        let cache_key = parsed.segments[..i].join("/");
        let repo_url = format!("https://{cache_key}");
        if git_ls_remote(&repo_url) {
            let subpath = if i < parsed.segments.len() {
                Some(parsed.segments[i..].join("/"))
            } else {
                None
            };
            return Ok(ResolvedRepo {
                cache_key,
                repo_url,
                subpath,
            });
        }
    }
    Err(ImportError::NoRepoFound {
        url: parsed.url.clone(),
    })
}

/// Returns true if `git ls-remote` against `url` exits zero (= repo exists).
fn git_ls_remote(url: &str) -> bool {
    Command::new("git")
        .args(["ls-remote", "--exit-code", "--quiet", url, "HEAD"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Positive grammar for a bare git import: `<host>('/' <segment>)+` with an
/// optional trailing `.git` and trailing slashes ignored.
fn is_valid_import_url(url: &str) -> bool {
    let trimmed = url.trim_end_matches('/');
    let body = trimmed.strip_suffix(".git").unwrap_or(trimmed).trim_end_matches('/');
    let mut segments = body.split('/');
    let Some(host) = segments.next() else {
        return false;
    };
    if !is_valid_host(host) {
        return false;
    }
    let mut have_path = false;
    for seg in segments {
        if !is_valid_path_segment(seg) {
            return false;
        }
        have_path = true;
    }
    have_path
}

/// Host = at least one `.` and only DNS-safe characters. Excludes `:`, `@`,
/// and `/` so anything with a scheme or SSH shorthand fails here.
fn is_valid_host(s: &str) -> bool {
    !s.is_empty() && s.contains('.') && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
}

/// Path segment = unreserved URL chars (RFC 3986 unreserved minus `/`).
fn is_valid_path_segment(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'))
}

/// Derive the default provider name from a parsed URL: the last segment of
/// the URL (subpath if present, else the repo name).
fn provider_name_from_url(parsed: &ParsedImport) -> &str {
    parsed
        .segments
        .last()
        .expect("validated URL has at least 2 segments")
        .as_str()
}

/// `BUILD.bit.lock` contents. Flat TOML map of `cache_key` -> resolved
/// commit SHA. Wrapped as a transparent newtype so the file format stays
/// `"github.com/foo/bar" = "<sha>"` at the top level.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LockFile(pub BTreeMap<String, String>);

impl LockFile {
    /// Path of the lock file inside a project root.
    pub fn path(project_root: &Path) -> PathBuf {
        project_root.join(LOCK_FILENAME)
    }

    /// Load the lock file from the project root. A missing file is treated
    /// as an empty lock.
    pub fn load(project_root: &Path) -> Result<Self, ImportError> {
        let path = Self::path(project_root);
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path).map_err(|e| ImportError::LockRead(path.clone(), e))?;
        toml::from_str(&text).map_err(|e| ImportError::LockParse(path, e))
    }

    /// Persist the lock file. An empty lock removes the on-disk file so we
    /// don't leave a stale empty TOML in the project.
    pub fn save(&self, project_root: &Path) -> Result<(), ImportError> {
        let path = Self::path(project_root);
        if self.0.is_empty() {
            if path.exists() {
                fs::remove_file(&path).map_err(|e| ImportError::LockWrite(path, e))?;
            }
            return Ok(());
        }
        let text = toml::to_string(self).map_err(ImportError::LockSerialize)?;
        fs::write(&path, text).map_err(|e| ImportError::LockWrite(path, e))
    }
}

/// How `resolve_imports` should treat existing lock entries.
#[derive(Debug, Clone, Default)]
pub enum UpdateMode {
    /// Honour the lock; only resolve refs for repos with no lock entry yet.
    #[default]
    None,
    /// Re-resolve refs and rewrite the lock. `Some(filter)` restricts the
    /// re-resolution to matching `cache_key`s; `None` updates all repos.
    Update(Option<Vec<String>>),
}

/// One line of the change summary printed by `bit --update`.
#[derive(Debug, Clone, PartialEq)]
pub struct LockChange {
    pub repo: String,
    pub old: Option<String>,
    pub new: String,
}

/// A single resolved import: one provider, exposing the `<resource>.bit` files
/// at the root of `path`.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportRoot {
    /// Provider name (last path segment of the import URL or local path).
    pub provider: String,
    /// Directory containing this provider's `<resource>.bit` files.
    pub path: PathBuf,
}

/// Result of `resolve_imports`.
#[derive(Debug, Clone, Default)]
pub struct ImportResolution {
    /// All providers brought into scope by the root project and its transitive
    /// imports.
    pub roots: Vec<ImportRoot>,
    /// Lock entries that changed during this resolution (only ever populated
    /// from the root project's lock — child locks are read-only).
    pub changes: Vec<LockChange>,
    /// `--update` filter strings that didn't match any git import. Empty in
    /// non-update mode. Callers should warn the user about typos.
    pub unmatched_filters: Vec<String>,
}

/// Default bit cache root (`<user cache dir>/bit`).
pub fn default_cache_root() -> Result<PathBuf, ImportError> {
    Ok(dirs::cache_dir().ok_or(ImportError::NoCacheDir)?.join("bit"))
}

/// Returns true if the URL looks like a relative local filesystem path
/// (anchored at `.`, e.g. `./modules` or `../shared`). Absolute paths are
/// rejected by `parse_import_url` so non-portable imports don't sneak in.
fn is_local_path(url: &str) -> bool {
    url.starts_with('.')
}

/// In-progress resolution state, threaded through the recursive walk.
struct ResolveCtx {
    cache_root: PathBuf,
    /// provider name -> human-readable source description (for conflict messages).
    providers: BTreeMap<String, String>,
    /// git `cache_key` -> (resolved sha, source description).
    git_shas: BTreeMap<String, (String, String)>,
    /// Canonical path -> provider name already registered there. Lets us
    /// dedup duplicate identical imports while still catching the case of
    /// the same path being given two different provider names.
    path_providers: BTreeMap<PathBuf, String>,
    /// Canonical project directories whose `BUILD.bit` we've already
    /// descended into.
    visited_projects: BTreeSet<PathBuf>,
    /// Probed boundaries for non-forge URLs, cached for the run.
    probes: ProbeCache,
    /// Filter strings (from `--update <repo>...`) that haven't matched a git
    /// import yet. Caller can warn about leftovers.
    unmatched_filters: BTreeSet<String>,
    /// Roots in BFS order: root project first, then transitively imported.
    roots: Vec<ImportRoot>,
}

impl ResolveCtx {
    fn new(cache_root: &Path) -> Self {
        Self {
            cache_root: cache_root.to_owned(),
            providers: BTreeMap::new(),
            git_shas: BTreeMap::new(),
            path_providers: BTreeMap::new(),
            visited_projects: BTreeSet::new(),
            probes: ProbeCache::default(),
            unmatched_filters: BTreeSet::new(),
            roots: Vec::new(),
        }
    }

    /// Register a local-import-derived root. Returns true if newly added,
    /// false if it was a duplicate (caller can skip enqueueing for recursion).
    /// When `alias` is `Some`, it overrides the path-derived provider name.
    /// Errors if the target directory doesn't exist.
    fn add_local_root(&mut self, path: &Path, original: &str, alias: Option<&str>) -> Result<bool, ImportError> {
        if !path.is_dir() {
            return Err(ImportError::LocalImportMissing {
                path: original.to_owned(),
            });
        }
        let provider = match alias {
            Some(a) => a.to_owned(),
            None => derive_local_provider(path, original)?,
        };
        self.try_add(path, provider, format!("local: {original}"))
    }

    /// Register a git-import-derived provider, checking for SHA conflicts on
    /// the same `cache_key`. The same repo may be imported at multiple
    /// subpaths — each contributes a separate `ImportRoot`, but shares one
    /// lock entry.
    fn add_git_provider(
        &mut self,
        cache_key: &str,
        sha: &str,
        provider_path: &Path,
        provider: String,
        original_url: &str,
    ) -> Result<bool, ImportError> {
        match self.git_shas.get(cache_key) {
            Some((existing_sha, existing_source)) if existing_sha != sha => {
                return Err(ImportError::ShaConflict {
                    provider,
                    first_sha: existing_sha.clone(),
                    first_source: existing_source.clone(),
                    second_sha: sha.to_owned(),
                    second_source: format!("git: {original_url}"),
                });
            }
            Some(_) => {}
            None => {
                self.git_shas
                    .insert(cache_key.to_owned(), (sha.to_owned(), format!("git: {original_url}")));
            }
        }
        self.try_add(provider_path, provider, format!("git: {original_url}"))
    }

    /// Add an `ImportRoot`. Returns `Ok(false)` on exact-duplicate (same
    /// path, same provider); errors on `AliasMismatch` (same path, different
    /// provider) or `ProviderConflict` (different paths, same provider).
    fn try_add(&mut self, path: &Path, provider: String, source: String) -> Result<bool, ImportError> {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_owned());
        if let Some(existing_provider) = self.path_providers.get(&canonical) {
            if existing_provider == &provider {
                return Ok(false);
            }
            return Err(ImportError::AliasMismatch {
                path: canonical.display().to_string(),
                first: existing_provider.clone(),
                second: provider,
            });
        }
        if let Some(existing_source) = self.providers.get(&provider) {
            return Err(ImportError::ProviderConflict {
                provider,
                first: existing_source.clone(),
                second: source,
            });
        }
        self.path_providers.insert(canonical.clone(), provider.clone());
        self.providers.insert(provider.clone(), source);
        self.roots.push(ImportRoot {
            provider,
            path: canonical,
        });
        Ok(true)
    }
}

/// Provider name = the last segment of the resolved local path. Rejects
/// non-name segments like `.` and `..`.
fn derive_local_provider(path: &Path, original: &str) -> Result<String, ImportError> {
    path.file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty() && *s != "." && *s != "..")
        .map(String::from)
        .ok_or_else(|| ImportError::InvalidProvider {
            path: original.to_owned(),
        })
}

/// Resolve every `import` statement in `module` plus every transitively
/// imported project's `import` statements.
///
/// The root project's `BUILD.bit.lock` is writable: new git imports are
/// auto-pinned, and `mode = Update(...)` re-resolves matching entries.
/// Each imported project's `BUILD.bit.lock` is read-only and **must** carry
/// an entry for every git import that project declares.
///
/// # Errors
///
/// - URL parse errors, git failures, lock file IO / serialization failures.
/// - `ProviderConflict` when two unrelated imports want the same provider name.
/// - `ShaConflict` when the same git repo is required at two different SHAs.
/// - `MissingLockEntry` when a child project's git import has no lock entry.
pub fn resolve_imports(
    root_module: &Module,
    project_root: &Path,
    cache_root: &Path,
    mode: UpdateMode,
) -> Result<ImportResolution, ImportError> {
    let mut ctx = ResolveCtx::new(cache_root);
    if let UpdateMode::Update(Some(filter)) = &mode {
        ctx.unmatched_filters = filter.iter().cloned().collect();
    }
    let mut queue: VecDeque<PathBuf> = VecDeque::new();

    let old_lock = LockFile::load(project_root)?;
    let (new_lock, changes) = process_root_project(root_module, project_root, &mut ctx, &mut queue, &old_lock, &mode)?;
    if new_lock != old_lock {
        new_lock.save(project_root)?;
    }
    ctx.visited_projects
        .insert(project_root.canonicalize().unwrap_or_else(|_| project_root.to_owned()));

    while let Some(dir) = queue.pop_front() {
        let canonical = dir.canonicalize().unwrap_or(dir);
        if !ctx.visited_projects.insert(canonical.clone()) {
            continue;
        }
        process_child_project(&canonical, &mut ctx, &mut queue)?;
    }

    Ok(ImportResolution {
        roots: ctx.roots,
        changes,
        unmatched_filters: ctx.unmatched_filters.into_iter().collect(),
    })
}

/// Process the root project: walks its imports, materializes git deps,
/// updates the writable lock, enqueues each resolved repo for recursion.
fn process_root_project(
    module: &Module,
    project_dir: &Path,
    ctx: &mut ResolveCtx,
    queue: &mut VecDeque<PathBuf>,
    old_lock: &LockFile,
    mode: &UpdateMode,
) -> Result<(LockFile, Vec<LockChange>), ImportError> {
    let mut new_lock = LockFile::default();
    let mut changes = Vec::new();

    for stmt in &module.statements {
        let Statement::Import(imp) = stmt else { continue };
        if is_local_path(&imp.url) {
            let target = project_dir.join(&imp.url);
            if ctx.add_local_root(&target, &imp.url, imp.alias.as_deref())? {
                queue.push_back(target);
            }
            continue;
        }
        let parsed = parse_import_url(&imp.url)?;
        let resolved = resolve_repo_boundary(&parsed, &mut ctx.probes)?;
        let old_sha = old_lock.0.get(&resolved.cache_key).cloned();
        let target_sha = match (
            &old_sha,
            should_update(mode, &resolved.cache_key, &mut ctx.unmatched_filters),
        ) {
            (Some(sha), false) => sha.clone(),
            _ => resolve_to_sha(&ctx.cache_root, &resolved.cache_key, &resolved.repo_url)?,
        };
        let repo_path = ensure_materialized(&ctx.cache_root, &resolved.cache_key, &resolved.repo_url, &target_sha)?;
        if old_sha.as_deref() != Some(target_sha.as_str()) {
            changes.push(LockChange {
                repo: resolved.cache_key.clone(),
                old: old_sha,
                new: target_sha.clone(),
            });
        }
        new_lock.0.insert(resolved.cache_key.clone(), target_sha.clone());
        let (provider_path, provider) = provider_target(&repo_path, &parsed, &resolved, imp.alias.as_deref());
        if ctx.add_git_provider(&resolved.cache_key, &target_sha, &provider_path, provider, &imp.url)? {
            // Recurse on the repo root (where BUILD.bit lives), not the subpath.
            queue.push_back(repo_path);
        }
    }

    Ok((new_lock, changes))
}

/// Process a transitively-imported project (read-only lock). Skips silently
/// if the directory has no `BUILD.bit` — a leaf provider repo with no deps.
fn process_child_project(
    project_dir: &Path,
    ctx: &mut ResolveCtx,
    queue: &mut VecDeque<PathBuf>,
) -> Result<(), ImportError> {
    let bit_path = project_dir.join("BUILD.bit");
    if !bit_path.exists() {
        return Ok(());
    }
    let source = fs::read_to_string(&bit_path).map_err(|e| ImportError::ChildRead {
        path: bit_path.display().to_string(),
        source: e,
    })?;
    let module =
        crate::parser::parse(&source, &bit_path.display().to_string()).map_err(|e| ImportError::ChildParse {
            path: bit_path.display().to_string(),
            message: e.message,
        })?;
    let lock = LockFile::load(project_dir)?;

    for stmt in &module.statements {
        let Statement::Import(imp) = stmt else { continue };
        if is_local_path(&imp.url) {
            let target = project_dir.join(&imp.url);
            if ctx.add_local_root(&target, &imp.url, imp.alias.as_deref())? {
                queue.push_back(target);
            }
            continue;
        }
        let parsed = parse_import_url(&imp.url)?;
        let resolved = resolve_repo_boundary(&parsed, &mut ctx.probes)?;
        let sha = lock
            .0
            .get(&resolved.cache_key)
            .ok_or_else(|| ImportError::MissingLockEntry {
                project: project_dir.display().to_string(),
                repo: resolved.cache_key.clone(),
            })?
            .clone();
        let repo_path = ensure_materialized(&ctx.cache_root, &resolved.cache_key, &resolved.repo_url, &sha)?;
        let (provider_path, provider) = provider_target(&repo_path, &parsed, &resolved, imp.alias.as_deref());
        if ctx.add_git_provider(&resolved.cache_key, &sha, &provider_path, provider, &imp.url)? {
            queue.push_back(repo_path);
        }
    }

    Ok(())
}

/// Compute the on-disk path and provider name for one git import.
fn provider_target(
    repo_path: &Path,
    parsed: &ParsedImport,
    resolved: &ResolvedRepo,
    alias: Option<&str>,
) -> (PathBuf, String) {
    let provider_path = match &resolved.subpath {
        Some(s) => repo_path.join(s),
        None => repo_path.to_owned(),
    };
    let provider = alias
        .map(String::from)
        .unwrap_or_else(|| provider_name_from_url(parsed).to_owned());
    (provider_path, provider)
}

/// Decide if a given repo should be re-resolved according to the update
/// mode. Marks any matched filter strings as "used" so the caller can later
/// warn about filters that didn't match anything.
fn should_update(mode: &UpdateMode, cache_key: &str, unmatched: &mut BTreeSet<String>) -> bool {
    match mode {
        UpdateMode::None => false,
        UpdateMode::Update(None) => true,
        UpdateMode::Update(Some(filter)) => {
            let mut any = false;
            for f in filter {
                if filter_matches_key(f, cache_key) {
                    any = true;
                    unmatched.remove(f);
                }
            }
            any
        }
    }
}

/// Match a user-supplied filter against a cache key. Accepts the bare
/// `cache_key` (e.g. `github.com/foo/bar`), the same key with `.git` /
/// trailing slashes, or a full subpath import URL whose repo prefix is the
/// cache key.
fn filter_matches_key(filter: &str, cache_key: &str) -> bool {
    let trimmed = filter.trim_end_matches('/');
    let stripped = trimmed.strip_suffix(".git").unwrap_or(trimmed).trim_end_matches('/');
    if stripped == cache_key {
        return true;
    }
    stripped
        .strip_prefix(cache_key)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Path to the bare clone of a repo.
fn bare_clone_path(cache_root: &Path, cache_key: &str) -> PathBuf {
    cache_root.join(GIT_CACHE_SUBDIR).join(format!("{cache_key}.git"))
}

/// Path to the extracted, immutable working tree for a specific commit.
fn module_tree_path(cache_root: &Path, cache_key: &str, sha: &str) -> PathBuf {
    cache_root.join(MOD_CACHE_SUBDIR).join(cache_key).join(sha)
}

/// Resolve the remote's default-branch HEAD to a full commit SHA. Ensures
/// the bare clone exists and is fetched so the SHA is then materializable
/// without another network round trip.
fn resolve_to_sha(cache_root: &Path, cache_key: &str, repo_url: &str) -> Result<String, ImportError> {
    let bare = bare_clone_path(cache_root, cache_key);
    ensure_bare_clone(repo_url, &bare)?;
    git_fetch(&bare, repo_url)?;
    git_default_branch_sha(repo_url)
}

/// Make sure `<dest>` is a fully-extracted working tree for `sha`. If the
/// dest already exists we trust it (immutable by convention). Otherwise we
/// extract via `git archive <sha> | tar -x` into a temp dir and atomically
/// rename it into place.
fn ensure_materialized(cache_root: &Path, cache_key: &str, repo_url: &str, sha: &str) -> Result<PathBuf, ImportError> {
    let dest = module_tree_path(cache_root, cache_key, sha);
    if dest.is_dir() {
        return Ok(dest);
    }
    let bare = bare_clone_path(cache_root, cache_key);
    ensure_bare_clone(repo_url, &bare)?;
    if !git_has_commit(&bare, sha) {
        git_fetch(&bare, repo_url)?;
    }
    if !git_has_commit(&bare, sha) {
        return Err(ImportError::GitArchive {
            repo_url: repo_url.to_owned(),
            sha: sha.to_owned(),
            message: "commit not found in repository after fetch".to_owned(),
        });
    }

    let parent = dest.parent().expect("module tree path has a parent");
    fs::create_dir_all(parent)?;
    // Per-process temp suffix avoids collisions between concurrent `bit` runs
    // racing to materialize the same commit.
    let tmp = parent.join(format!("{sha}.tmp.{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;

    let extract_result = extract_commit(&bare, repo_url, sha, &tmp);
    if let Err(e) = extract_result {
        let _ = fs::remove_dir_all(&tmp);
        return Err(e);
    }

    match fs::rename(&tmp, &dest) {
        Ok(()) => Ok(dest),
        // Lost the race; another process populated `dest` first.
        Err(_) if dest.is_dir() => {
            let _ = fs::remove_dir_all(&tmp);
            Ok(dest)
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&tmp);
            Err(e.into())
        }
    }
}

/// Stream `git archive <sha>` into `tar -x -C <dest>`.
fn extract_commit(bare: &Path, repo_url: &str, sha: &str, dest: &Path) -> Result<(), ImportError> {
    let mk_err = |message: String| ImportError::GitArchive {
        repo_url: repo_url.to_owned(),
        sha: sha.to_owned(),
        message,
    };

    let mut git = Command::new("git")
        .arg("--git-dir")
        .arg(bare)
        .args(["archive", "--format=tar", sha])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| mk_err(format!("spawn git: {e}")))?;
    let stdout = git.stdout.take().expect("git archive stdout piped");

    let tar = Command::new("tar")
        .args(["-x", "-f", "-", "-C"])
        .arg(dest)
        .stdin(Stdio::from(stdout))
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| mk_err(format!("spawn tar: {e}")))?;

    let git_out = git.wait_with_output().map_err(|e| mk_err(format!("wait git: {e}")))?;
    let tar_out = tar.wait_with_output().map_err(|e| mk_err(format!("wait tar: {e}")))?;
    if !git_out.status.success() || !tar_out.status.success() {
        return Err(mk_err(format!(
            "git={} stderr={}; tar={} stderr={}",
            git_out.status,
            String::from_utf8_lossy(&git_out.stderr).trim(),
            tar_out.status,
            String::from_utf8_lossy(&tar_out.stderr).trim(),
        )));
    }
    Ok(())
}

/// Create a bare clone at `bare_dir` if it doesn't already exist.
fn ensure_bare_clone(repo_url: &str, bare_dir: &Path) -> Result<(), ImportError> {
    if bare_dir.join("HEAD").is_file() {
        return Ok(());
    }
    if let Some(parent) = bare_dir.parent() {
        fs::create_dir_all(parent)?;
    }
    let out = Command::new("git")
        .args(["clone", "--bare", "--quiet", repo_url])
        .arg(bare_dir)
        .output()
        .map_err(|e| ImportError::GitClone {
            repo_url: repo_url.to_owned(),
            message: e.to_string(),
        })?;
    if !out.status.success() {
        return Err(ImportError::GitClone {
            repo_url: repo_url.to_owned(),
            message: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    Ok(())
}

/// `git fetch origin` updates all branch/tag refs in the bare clone.
fn git_fetch(bare_dir: &Path, repo_url: &str) -> Result<(), ImportError> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(bare_dir)
        .args(["fetch", "--quiet", "--tags", "--prune", "origin"])
        .output()
        .map_err(|e| ImportError::GitFetch {
            repo_url: repo_url.to_owned(),
            message: e.to_string(),
        })?;
    if !out.status.success() {
        return Err(ImportError::GitFetch {
            repo_url: repo_url.to_owned(),
            message: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    Ok(())
}

/// Check whether a commit object is already present in the bare clone.
fn git_has_commit(bare_dir: &Path, sha: &str) -> bool {
    Command::new("git")
        .arg("--git-dir")
        .arg(bare_dir)
        .args(["cat-file", "-e"])
        .arg(format!("{sha}^{{commit}}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Ask the remote (via `ls-remote`) what its current default-branch SHA is.
/// We don't trust the bare clone's HEAD because it's set once at clone time
/// and doesn't track the remote's default branch moves.
fn git_default_branch_sha(repo_url: &str) -> Result<String, ImportError> {
    let out = Command::new("git")
        .args(["ls-remote", "--symref", repo_url, "HEAD"])
        .output()
        .map_err(|e| ImportError::GitDefaultBranch {
            repo_url: repo_url.to_owned(),
            message: e.to_string(),
        })?;
    if !out.status.success() {
        return Err(ImportError::GitDefaultBranch {
            repo_url: repo_url.to_owned(),
            message: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    // `git ls-remote --symref <url> HEAD` prints two lines:
    //     ref: refs/heads/main\tHEAD
    //     <sha>\tHEAD
    // Skip the symref line and return the SHA from the second.
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if line.starts_with("ref:") {
            continue;
        }
        if let Some((sha, rest)) = line.split_once('\t')
            && rest.trim() == "HEAD"
        {
            return Ok(sha.trim().to_owned());
        }
    }
    Err(ImportError::GitDefaultBranch {
        repo_url: repo_url.to_owned(),
        message: format!("no HEAD line in ls-remote output: {text}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Import, Pos};

    #[test]
    fn parse_bare_github() {
        let parsed = parse_import_url("github.com/user/repo").unwrap();
        assert_eq!(parsed.url, "github.com/user/repo");
        assert_eq!(parsed.segments, vec!["github.com", "user", "repo"]);
    }

    #[test]
    fn parse_arbitrary_host() {
        let parsed = parse_import_url("git.sr.ht/~user/repo").unwrap();
        assert_eq!(parsed.url, "git.sr.ht/~user/repo");
        assert_eq!(parsed.segments, vec!["git.sr.ht", "~user", "repo"]);
    }

    #[test]
    fn parse_strips_trailing_dot_git_and_slashes() {
        let parsed = parse_import_url("github.com/user/repo.git/").unwrap();
        assert_eq!(parsed.url, "github.com/user/repo");
        assert_eq!(parsed.segments, vec!["github.com", "user", "repo"]);
    }

    #[test]
    fn forge_boundary_extracts_subpath() {
        let parsed = parse_import_url("github.com/foo/bar/waz/sub").unwrap();
        let resolved = forge_boundary(&parsed).unwrap();
        assert_eq!(resolved.cache_key, "github.com/foo/bar");
        assert_eq!(resolved.repo_url, "https://github.com/foo/bar");
        assert_eq!(resolved.subpath.as_deref(), Some("waz/sub"));
    }

    #[test]
    fn forge_boundary_no_subpath() {
        let parsed = parse_import_url("github.com/foo/bar").unwrap();
        let resolved = forge_boundary(&parsed).unwrap();
        assert_eq!(resolved.cache_key, "github.com/foo/bar");
        assert_eq!(resolved.subpath, None);
    }

    #[test]
    fn provider_name_uses_last_segment() {
        let p = parse_import_url("github.com/foo/bar").unwrap();
        assert_eq!(provider_name_from_url(&p), "bar");
        let p = parse_import_url("github.com/foo/bar/waz").unwrap();
        assert_eq!(provider_name_from_url(&p), "waz");
        let p = parse_import_url("git.example.com/dept/team/proj").unwrap();
        assert_eq!(provider_name_from_url(&p), "proj");
    }

    /// Every shape that isn't bare `host/path` falls out of the grammar.
    #[test]
    fn parse_rejects_non_bare_forms() {
        for bad in [
            "",
            "https://github.com/foo/bar",
            "http://github.com/foo/bar",
            "ssh://git@host/foo/bar",
            "file:///tmp/foo",
            "git@github.com:foo/bar.git",
            "github.com",          // no path
            "github/foo/bar",      // host without a dot
            "github.com/foo bar",  // space in path
            "github.com/foo?x=1",  // query
            "github.com/foo#frag", // fragment
        ] {
            assert!(
                matches!(parse_import_url(bad), Err(ImportError::InvalidUrl { .. })),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn filter_matches_subpath_url() {
        assert!(filter_matches_key("github.com/foo/bar", "github.com/foo/bar"));
        assert!(filter_matches_key("github.com/foo/bar.git", "github.com/foo/bar"));
        assert!(filter_matches_key("github.com/foo/bar/waz", "github.com/foo/bar"));
        assert!(filter_matches_key("github.com/foo/bar/waz/sub", "github.com/foo/bar"));
        // Substring without slash boundary doesn't match.
        assert!(!filter_matches_key("github.com/foo/barbar", "github.com/foo/bar"));
        assert!(!filter_matches_key("github.com/foo/baz", "github.com/foo/bar"));
    }

    #[test]
    fn lockfile_roundtrip() {
        let mut lock = LockFile::default();
        lock.0.insert("github.com/a/b".into(), "abc123".into());
        lock.0.insert("gitlab.com/x/y".into(), "def456".into());
        let dir = tempfile::tempdir().unwrap();
        lock.save(dir.path()).unwrap();
        let loaded = LockFile::load(dir.path()).unwrap();
        assert_eq!(loaded, lock);
    }

    #[test]
    fn lockfile_load_missing_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = LockFile::load(dir.path()).unwrap();
        assert_eq!(loaded, LockFile::default());
    }

    #[test]
    fn lockfile_save_empty_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut lock = LockFile::default();
        lock.0.insert("github.com/a/b".into(), "abc".into());
        lock.save(dir.path()).unwrap();
        assert!(LockFile::path(dir.path()).exists());
        let empty = LockFile::default();
        empty.save(dir.path()).unwrap();
        assert!(!LockFile::path(dir.path()).exists());
    }

    #[test]
    fn lockfile_format_is_flat_top_level_map() {
        let mut lock = LockFile::default();
        lock.0.insert("github.com/a/b".into(), "abc".into());
        let dir = tempfile::tempdir().unwrap();
        lock.save(dir.path()).unwrap();
        let text = fs::read_to_string(LockFile::path(dir.path())).unwrap();
        assert!(
            text.contains(r#""github.com/a/b" = "abc""#),
            "unexpected lock format: {text}"
        );
    }

    #[test]
    fn is_local_path_detects_relative_only() {
        assert!(is_local_path("./.bit/modules"));
        assert!(is_local_path("../shared"));
        assert!(!is_local_path("/abs/path"));
        assert!(!is_local_path("github.com/foo/bar"));
    }

    #[test]
    fn parse_rejects_absolute_path() {
        assert!(matches!(
            parse_import_url("/abs/path"),
            Err(ImportError::InvalidUrl { .. })
        ));
    }

    #[test]
    fn filter_matches_with_dot_git_suffix() {
        assert!(filter_matches_key("github.com/a/b", "github.com/a/b"));
        assert!(filter_matches_key("github.com/a/b.git", "github.com/a/b"));
        assert!(!filter_matches_key("github.com/a/c", "github.com/a/b"));
    }

    /// Build a module with just import statements, for the local-path flow.
    fn module_with_imports(urls: &[&str]) -> Module {
        Module {
            doc: None,
            statements: urls
                .iter()
                .map(|u| {
                    Statement::Import(Import {
                        pos: Pos::default(),
                        url: (*u).to_owned(),
                        alias: None,
                    })
                })
                .collect(),
        }
    }

    fn module_with_aliased_imports(pairs: &[(&str, Option<&str>)]) -> Module {
        Module {
            doc: None,
            statements: pairs
                .iter()
                .map(|(u, a)| {
                    Statement::Import(Import {
                        pos: Pos::default(),
                        url: (*u).to_owned(),
                        alias: a.map(String::from),
                    })
                })
                .collect(),
        }
    }

    #[test]
    fn alias_overrides_path_derived_provider_name() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("vendor/foo")).unwrap();
        let module = module_with_aliased_imports(&[("./vendor/foo", Some("bar"))]);
        let res = resolve_imports(&module, project.path(), cache.path(), UpdateMode::None).unwrap();
        let providers: Vec<&str> = res.roots.iter().map(|r| r.provider.as_str()).collect();
        assert_eq!(providers, vec!["bar"]);
    }

    #[test]
    fn alias_resolves_conflicting_provider_names() {
        // Two imports whose last segments collide are normally an error;
        // adding an `as` clause to one resolves the conflict.
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("a/docker")).unwrap();
        fs::create_dir_all(project.path().join("b/docker")).unwrap();
        let module = module_with_aliased_imports(&[("./a/docker", None), ("./b/docker", Some("docker-fork"))]);
        let res = resolve_imports(&module, project.path(), cache.path(), UpdateMode::None).unwrap();
        let providers: Vec<&str> = res.roots.iter().map(|r| r.provider.as_str()).collect();
        assert_eq!(providers, vec!["docker", "docker-fork"]);
    }

    #[test]
    fn resolve_imports_local_only_preserves_order() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("a")).unwrap();
        fs::create_dir_all(project.path().join("b/c")).unwrap();
        let module = module_with_imports(&["./a", "./b/c"]);
        let res = resolve_imports(&module, project.path(), cache.path(), UpdateMode::None).unwrap();
        let providers: Vec<&str> = res.roots.iter().map(|r| r.provider.as_str()).collect();
        assert_eq!(providers, vec!["a", "c"]);
        assert!(res.changes.is_empty());
        assert!(!LockFile::path(project.path()).exists());
    }

    #[test]
    fn resolve_imports_errors_on_provider_conflict() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        // Two local imports whose last segments collide.
        fs::create_dir_all(project.path().join("a/docker")).unwrap();
        fs::create_dir_all(project.path().join("b/docker")).unwrap();
        let module = module_with_imports(&["./a/docker", "./b/docker"]);
        let err = resolve_imports(&module, project.path(), cache.path(), UpdateMode::None).unwrap_err();
        assert!(matches!(err, ImportError::ProviderConflict { .. }), "got: {err}");
    }

    #[test]
    fn derive_local_provider_rejects_dot_segments() {
        assert!(derive_local_provider(Path::new("."), ".").is_err());
        assert!(derive_local_provider(Path::new(".."), "..").is_err());
    }

    #[test]
    fn forge_boundary_requires_three_segments() {
        let parsed = parse_import_url("github.com/foo").unwrap_or_else(|_| ParsedImport {
            url: "github.com/foo".into(),
            segments: vec!["github.com".into(), "foo".into()],
        });
        // grammar already rejects host with a single segment, so build manually.
        assert!(matches!(forge_boundary(&parsed), Err(ImportError::InvalidUrl { .. })));
    }

    /// Initialize a local source repo with one commit. Returns (repo path, commit SHA).
    fn make_local_repo() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        run_git(p, &["init", "--quiet", "--initial-branch=main"]);
        run_git(p, &["config", "user.email", "test@example.com"]);
        run_git(p, &["config", "user.name", "Test"]);
        fs::write(p.join("hello.txt"), "hi\n").unwrap();
        run_git(p, &["add", "."]);
        run_git(p, &["commit", "--quiet", "-m", "initial"]);
        let out = Command::new("git")
            .current_dir(p)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let sha = String::from_utf8(out.stdout).unwrap().trim().to_owned();
        (dir, sha)
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let out = Command::new("git").current_dir(dir).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn materialize_and_resolve_against_local_repo() {
        let (src, sha) = make_local_repo();
        let cache = tempfile::tempdir().unwrap();
        let key = "local/test/repo";
        let repo_url = src.path().to_str().unwrap();

        let resolved = resolve_to_sha(cache.path(), key, repo_url).unwrap();
        assert_eq!(resolved, sha);

        let extracted = ensure_materialized(cache.path(), key, repo_url, &sha).unwrap();
        assert!(extracted.join("hello.txt").is_file());

        // Idempotent: a second call short-circuits because dest exists.
        let again = ensure_materialized(cache.path(), key, repo_url, &sha).unwrap();
        assert_eq!(again, extracted);
    }

    #[test]
    fn default_branch_sha_matches_main() {
        let (src, sha) = make_local_repo();
        let resolved = git_default_branch_sha(src.path().to_str().unwrap()).unwrap();
        assert_eq!(resolved, sha);
    }

    #[test]
    fn recursive_local_transitive_imports() {
        // Root project imports ./mods/foo. foo/BUILD.bit imports ../bar.
        // Resolver should surface both as providers.
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let foo = project.path().join("mods/foo");
        let bar = project.path().join("mods/bar");
        fs::create_dir_all(&foo).unwrap();
        fs::create_dir_all(&bar).unwrap();
        fs::write(foo.join("BUILD.bit"), "import \"../bar\"\n").unwrap();

        let module = module_with_imports(&["./mods/foo"]);
        let res = resolve_imports(&module, project.path(), cache.path(), UpdateMode::None).unwrap();
        let providers: Vec<&str> = res.roots.iter().map(|r| r.provider.as_str()).collect();
        assert_eq!(providers, vec!["foo", "bar"]);
    }

    #[test]
    fn child_project_without_build_bit_is_a_leaf() {
        // A resolved import with no BUILD.bit is treated as a leaf — no
        // recursion, no error.
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let foo = project.path().join("foo");
        fs::create_dir_all(&foo).unwrap();
        fs::write(foo.join("res.bit"), "").unwrap();

        let module = module_with_imports(&["./foo"]);
        let res = resolve_imports(&module, project.path(), cache.path(), UpdateMode::None).unwrap();
        assert_eq!(res.roots.len(), 1);
        assert_eq!(res.roots[0].provider, "foo");
    }

    #[test]
    fn update_mode_filter_matches_by_cache_key() {
        let mut un = BTreeSet::new();
        let m = UpdateMode::Update(Some(vec!["github.com/a/b".into()]));
        un.insert("github.com/a/b".into());
        assert!(should_update(&m, "github.com/a/b", &mut un));
        assert!(un.is_empty(), "matched filter should be drained");
        let mut un2 = BTreeSet::from(["github.com/a/b".into()]);
        assert!(!should_update(&m, "github.com/c/d", &mut un2));
        assert_eq!(un2.len(), 1, "non-match leaves filter in place");
        let m_all = UpdateMode::Update(None);
        let mut un3 = BTreeSet::new();
        assert!(should_update(&m_all, "anything", &mut un3));
        let m_none = UpdateMode::None;
        let mut un4 = BTreeSet::new();
        assert!(!should_update(&m_none, "github.com/a/b", &mut un4));
    }

    #[test]
    fn try_add_alias_mismatch_errors() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("foo")).unwrap();
        let module = module_with_aliased_imports(&[("./foo", Some("a")), ("./foo", Some("b"))]);
        let err = resolve_imports(&module, project.path(), cache.path(), UpdateMode::None).unwrap_err();
        assert!(matches!(err, ImportError::AliasMismatch { .. }), "got: {err}");
    }

    #[test]
    fn same_path_same_provider_dedups_silently() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("foo")).unwrap();
        let module = module_with_aliased_imports(&[("./foo", Some("bar")), ("./foo", Some("bar"))]);
        let res = resolve_imports(&module, project.path(), cache.path(), UpdateMode::None).unwrap();
        assert_eq!(res.roots.len(), 1);
    }

    #[test]
    fn local_import_missing_dir_errors() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let module = module_with_imports(&["./does-not-exist"]);
        let err = resolve_imports(&module, project.path(), cache.path(), UpdateMode::None).unwrap_err();
        assert!(matches!(err, ImportError::LocalImportMissing { .. }), "got: {err}");
    }

    #[test]
    fn missing_child_lock_entry_errors() {
        // The child has a git import (github.com, so no probing) but no
        // matching `BUILD.bit.lock` entry. The check fires before any
        // network call.
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let child = project.path().join("child");
        fs::create_dir_all(&child).unwrap();
        fs::write(child.join("BUILD.bit"), "import \"github.com/x/y\"\n").unwrap();
        let module = module_with_imports(&["./child"]);
        let err = resolve_imports(&module, project.path(), cache.path(), UpdateMode::None).unwrap_err();
        assert!(matches!(err, ImportError::MissingLockEntry { .. }), "got: {err}");
    }

    #[test]
    fn add_git_provider_errors_on_sha_conflict() {
        let cache = tempfile::tempdir().unwrap();
        let mut ctx = ResolveCtx::new(cache.path());
        ctx.add_git_provider("github.com/x/y", "sha1", cache.path(), "y".into(), "github.com/x/y")
            .unwrap();
        let err = ctx
            .add_git_provider("github.com/x/y", "sha2", cache.path(), "y".into(), "github.com/x/y")
            .unwrap_err();
        assert!(matches!(err, ImportError::ShaConflict { .. }), "got: {err}");
    }

    #[test]
    fn add_git_provider_allows_multi_subpath_same_repo() {
        let cache = tempfile::tempdir().unwrap();
        let repo = cache.path();
        let waz = repo.join("waz");
        let qux = repo.join("qux");
        fs::create_dir_all(&waz).unwrap();
        fs::create_dir_all(&qux).unwrap();
        let mut ctx = ResolveCtx::new(cache.path());
        assert!(
            ctx.add_git_provider("github.com/x/y", "sha", &waz, "waz".into(), "github.com/x/y/waz")
                .unwrap()
        );
        assert!(
            ctx.add_git_provider("github.com/x/y", "sha", &qux, "qux".into(), "github.com/x/y/qux")
                .unwrap()
        );
        assert_eq!(ctx.roots.len(), 2);
        assert_eq!(ctx.git_shas.len(), 1, "one repo, one lock entry");
    }

    #[test]
    fn provider_target_alias_overrides_full_url_segment() {
        let parsed = parse_import_url("github.com/foo/bar/waz").unwrap();
        let resolved = forge_boundary(&parsed).unwrap();
        let (path, provider) = provider_target(Path::new("/repo"), &parsed, &resolved, Some("alias"));
        assert_eq!(provider, "alias");
        assert_eq!(path, Path::new("/repo/waz"));
    }

    #[test]
    fn provider_target_default_uses_full_url_last_segment() {
        let parsed = parse_import_url("github.com/foo/bar/waz").unwrap();
        let resolved = forge_boundary(&parsed).unwrap();
        let (path, provider) = provider_target(Path::new("/repo"), &parsed, &resolved, None);
        assert_eq!(provider, "waz");
        assert_eq!(path, Path::new("/repo/waz"));
    }
}
