use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use thiserror::Error;

/// How long before we re-fetch a cached repo.
const CACHE_TTL: Duration = Duration::from_secs(3600);

/// Errors from import resolution.
#[derive(Debug, Error)]
pub enum ImportError {
    #[error("unsupported git host in import '{url}': only github.com, gitlab.com, and bitbucket.org are supported")]
    UnsupportedHost { url: String },
    #[error("import '{url}': expected at least owner/repo after host")]
    MissingRepo { url: String },
    #[error("git clone of {repo_url} failed: {message}")]
    GitClone { repo_url: String, message: String },
    #[error("git fetch of {repo_url} failed: {message}")]
    GitFetch { repo_url: String, message: String },
    #[error("git checkout of ref '{gitref}' failed: {message}")]
    GitCheckout { gitref: String, message: String },
    #[error("cannot determine cache directory")]
    NoCacheDir,
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// A parsed import URL split into its components.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedImport {
    /// The full git clone URL (e.g. "https://github.com/user/repo.git").
    pub repo_url: String,
    /// Host/owner/repo used as the cache key (e.g. "github.com/user/repo").
    pub cache_key: String,
    /// Optional subdirectory within the repo to use as module root.
    pub subpath: Option<String>,
    /// Optional git ref (branch, tag, commit).
    pub gitref: Option<String>,
}

/// Parse an import URL string into its components.
///
/// Format: `host/owner/repo[/subpath...][#ref]`
///
/// Only well-known forges are supported (github.com, gitlab.com, bitbucket.org).
/// For these, the repo boundary is always the first two path segments after the
/// host.
pub fn parse_import_url(url: &str) -> Result<ParsedImport, ImportError> {
    let (path_part, gitref) = match url.split_once('#') {
        Some((p, r)) => (p, Some(r.to_owned())),
        None => (url, None),
    };

    let segments: Vec<&str> = path_part.split('/').collect();
    if segments.len() < 3 {
        return Err(ImportError::MissingRepo { url: url.to_owned() });
    }

    let host = segments[0];
    match host {
        "github.com" | "gitlab.com" | "bitbucket.org" => {}
        _ => return Err(ImportError::UnsupportedHost { url: url.to_owned() }),
    }

    let owner = segments[1];
    let repo = segments[2];
    let cache_key = format!("{host}/{owner}/{repo}");
    let repo_url = format!("https://{host}/{owner}/{repo}.git");

    let subpath = if segments.len() > 3 {
        Some(segments[3..].join("/"))
    } else {
        None
    };

    Ok(ParsedImport {
        repo_url,
        cache_key,
        subpath,
        gitref,
    })
}

/// Resolve a parsed import to a local filesystem path, cloning/fetching as
/// needed.
///
/// Returns the path to the module root directory (repo root or subpath within
/// it).
pub fn resolve_import(parsed: &ParsedImport) -> Result<PathBuf, ImportError> {
    let cache_dir = dirs::cache_dir().ok_or(ImportError::NoCacheDir)?;
    let repo_dir = cache_dir.join("bit").join(&parsed.cache_key);

    if repo_dir.join(".git").is_dir() {
        fetch_if_stale(&repo_dir, &parsed.repo_url)?;
    } else {
        clone_repo(&parsed.repo_url, &repo_dir)?;
    }

    if let Some(ref gitref) = parsed.gitref {
        checkout_ref(&repo_dir, gitref)?;
    }

    let module_root = match &parsed.subpath {
        Some(sub) => repo_dir.join(sub),
        None => repo_dir,
    };

    Ok(module_root)
}

/// Clone a repo into the cache directory.
fn clone_repo(repo_url: &str, dest: &Path) -> Result<(), ImportError> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let output = Command::new("git")
        .args(["clone", "--quiet", repo_url])
        .arg(dest)
        .output()
        .map_err(|e| ImportError::GitClone {
            repo_url: repo_url.to_owned(),
            message: e.to_string(),
        })?;

    if !output.status.success() {
        return Err(ImportError::GitClone {
            repo_url: repo_url.to_owned(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    write_fetch_timestamp(dest)?;
    Ok(())
}

/// Fetch updates if the cache is older than CACHE_TTL.
fn fetch_if_stale(repo_dir: &Path, repo_url: &str) -> Result<(), ImportError> {
    let ts_file = repo_dir.join(".bit_fetch_timestamp");
    if let Ok(meta) = fs::metadata(&ts_file)
        && let Ok(modified) = meta.modified()
        && SystemTime::now().duration_since(modified).unwrap_or(CACHE_TTL) < CACHE_TTL
    {
        return Ok(());
    }

    let output = Command::new("git")
        .args(["fetch", "--quiet", "--all"])
        .current_dir(repo_dir)
        .output()
        .map_err(|e| ImportError::GitFetch {
            repo_url: repo_url.to_owned(),
            message: e.to_string(),
        })?;

    if !output.status.success() {
        return Err(ImportError::GitFetch {
            repo_url: repo_url.to_owned(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    // Update the default branch tracking after fetch
    let _ = Command::new("git")
        .args(["merge", "--ff-only", "--quiet"])
        .current_dir(repo_dir)
        .output();

    write_fetch_timestamp(repo_dir)?;
    Ok(())
}

/// Check out a specific ref (branch, tag, or commit).
fn checkout_ref(repo_dir: &Path, gitref: &str) -> Result<(), ImportError> {
    let output = Command::new("git")
        .args(["checkout", "--quiet", gitref])
        .current_dir(repo_dir)
        .output()
        .map_err(|e| ImportError::GitCheckout {
            gitref: gitref.to_owned(),
            message: e.to_string(),
        })?;

    if !output.status.success() {
        return Err(ImportError::GitCheckout {
            gitref: gitref.to_owned(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(())
}

fn write_fetch_timestamp(repo_dir: &Path) -> Result<(), io::Error> {
    fs::write(repo_dir.join(".bit_fetch_timestamp"), "")
}

/// Returns true if the URL looks like a local filesystem path.
fn is_local_path(url: &str) -> bool {
    url.starts_with('.') || url.starts_with('/')
}

/// Extract import statements from a module and resolve them to local paths.
///
/// Returns paths in declaration order (the caller should search them in
/// reverse so that later imports take priority).
///
/// Local paths (starting with `.` or `/`) are resolved relative to `root`.
/// Everything else is treated as a git URL.
pub fn resolve_imports(module: &crate::ast::Module, root: &Path) -> Result<Vec<PathBuf>, ImportError> {
    let mut roots = Vec::new();
    for stmt in &module.statements {
        if let crate::ast::Statement::Import(imp) = stmt {
            if is_local_path(&imp.url) {
                roots.push(root.join(&imp.url));
            } else {
                let parsed = parse_import_url(&imp.url)?;
                let path = resolve_import(&parsed)?;
                roots.push(path);
            }
        }
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_github_basic() {
        let parsed = parse_import_url("github.com/user/repo").unwrap();
        assert_eq!(parsed.repo_url, "https://github.com/user/repo.git");
        assert_eq!(parsed.cache_key, "github.com/user/repo");
        assert_eq!(parsed.subpath, None);
        assert_eq!(parsed.gitref, None);
    }

    #[test]
    fn parse_github_with_subpath() {
        let parsed = parse_import_url("github.com/user/repo/modules/aws").unwrap();
        assert_eq!(parsed.repo_url, "https://github.com/user/repo.git");
        assert_eq!(parsed.cache_key, "github.com/user/repo");
        assert_eq!(parsed.subpath, Some("modules/aws".into()));
        assert_eq!(parsed.gitref, None);
    }

    #[test]
    fn parse_github_with_ref() {
        let parsed = parse_import_url("github.com/user/repo#v1.0").unwrap();
        assert_eq!(parsed.repo_url, "https://github.com/user/repo.git");
        assert_eq!(parsed.gitref, Some("v1.0".into()));
    }

    #[test]
    fn parse_github_with_subpath_and_ref() {
        let parsed = parse_import_url("github.com/user/repo/path/to/mods#main").unwrap();
        assert_eq!(parsed.cache_key, "github.com/user/repo");
        assert_eq!(parsed.subpath, Some("path/to/mods".into()));
        assert_eq!(parsed.gitref, Some("main".into()));
    }

    #[test]
    fn parse_gitlab() {
        let parsed = parse_import_url("gitlab.com/org/project").unwrap();
        assert_eq!(parsed.repo_url, "https://gitlab.com/org/project.git");
        assert_eq!(parsed.cache_key, "gitlab.com/org/project");
    }

    #[test]
    fn parse_bitbucket() {
        let parsed = parse_import_url("bitbucket.org/team/repo").unwrap();
        assert_eq!(parsed.repo_url, "https://bitbucket.org/team/repo.git");
        assert_eq!(parsed.cache_key, "bitbucket.org/team/repo");
    }

    #[test]
    fn parse_unsupported_host() {
        let err = parse_import_url("example.com/user/repo").unwrap_err();
        assert!(err.to_string().contains("unsupported git host"));
    }

    #[test]
    fn parse_missing_repo() {
        let err = parse_import_url("github.com/user").unwrap_err();
        assert!(err.to_string().contains("expected at least owner/repo"));
    }

    #[test]
    fn parse_just_host() {
        let err = parse_import_url("github.com").unwrap_err();
        assert!(err.to_string().contains("expected at least owner/repo"));
    }
}
