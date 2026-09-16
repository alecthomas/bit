use std::path::{Path, PathBuf};

use crate::sha256::Hasher;

/// Identity under which a project's action receipts are stored.
///
/// Linked Git worktrees of one repository share receipts only when the
/// bit project sits at the same path relative to the worktree root, so
/// nested bit projects in one repository never collide. Outside Git the
/// canonical project path is the identity, which is equivalent to today's
/// per-path local state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectIdentity {
    Git {
        /// Canonical path of the repository's common `.git` directory.
        common_dir: PathBuf,
        /// Path from the worktree root to the project root.
        relative: PathBuf,
    },
    Path(PathBuf),
}

impl ProjectIdentity {
    /// Detect the identity of the project rooted at `root`.
    pub fn detect(root: &Path) -> Self {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        git_identity(&canonical).unwrap_or(ProjectIdentity::Path(canonical))
    }

    /// Root of the repository's main worktree, if this is a Git project with
    /// a conventional `.git` directory.
    pub fn main_worktree(&self) -> Option<PathBuf> {
        match self {
            ProjectIdentity::Git { common_dir, .. } if common_dir.file_name().is_some_and(|n| n == ".git") => {
                common_dir.parent().map(Path::to_path_buf)
            }
            _ => None,
        }
    }

    /// Stable hex identifier used as the receipt store directory name.
    pub fn id(&self) -> String {
        let mut hasher = Hasher::new();
        match self {
            ProjectIdentity::Git { common_dir, relative } => {
                hasher.update(b"git\0");
                hasher.update(common_dir.as_os_str().as_encoded_bytes());
                hasher.update(b"\0");
                hasher.update(relative.as_os_str().as_encoded_bytes());
            }
            ProjectIdentity::Path(path) => {
                hasher.update(b"path\0");
                hasher.update(path.as_os_str().as_encoded_bytes());
            }
        }
        hasher.finalize().to_string()
    }
}

fn git_identity(root: &Path) -> Option<ProjectIdentity> {
    let output = crate::git::command()
        .args(["rev-parse", "--git-common-dir", "--show-toplevel"])
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let common_dir = lines.next()?.trim();
    let toplevel = lines.next()?.trim();
    if common_dir.is_empty() || toplevel.is_empty() {
        return None;
    }
    // In the main worktree `--git-common-dir` is relative to the cwd.
    let common_dir = root.join(common_dir).canonicalize().ok()?;
    let toplevel = Path::new(toplevel).canonicalize().ok()?;
    let relative = root.strip_prefix(&toplevel).ok()?.to_path_buf();
    Some(ProjectIdentity::Git { common_dir, relative })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git(dir: &Path, args: &[&str]) -> bool {
        crate::git::command()
            .args(args)
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn init_repo(dir: &Path) -> bool {
        git(dir, &["init", "-q"])
            && git(
                dir,
                &[
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    "init",
                ],
            )
    }

    #[test]
    fn non_git_uses_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let id = ProjectIdentity::detect(dir.path());
        assert_eq!(id, ProjectIdentity::Path(dir.path().canonicalize().unwrap()));
        assert_eq!(id.main_worktree(), None);
    }

    #[test]
    fn linked_worktrees_share_identity_and_nested_projects_differ() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        fs::create_dir_all(main.join("nested")).unwrap();
        if !init_repo(&main) {
            eprintln!("git unavailable; skipping");
            return;
        }
        let linked = dir.path().join("linked");
        assert!(git(&main, &["worktree", "add", "-q", linked.to_str().unwrap()]));
        fs::create_dir_all(linked.join("nested")).unwrap();

        let a = ProjectIdentity::detect(&main);
        let b = ProjectIdentity::detect(&linked);
        assert!(matches!(a, ProjectIdentity::Git { .. }), "{a:?}");
        assert_eq!(a, b);
        assert_eq!(a.id(), b.id());

        assert_eq!(a.main_worktree(), Some(main.canonicalize().unwrap()));
        assert_eq!(b.main_worktree(), Some(main.canonicalize().unwrap()));

        let a_nested = ProjectIdentity::detect(&main.join("nested"));
        let b_nested = ProjectIdentity::detect(&linked.join("nested"));
        assert_eq!(a_nested, b_nested);
        assert_ne!(a, a_nested);
        assert_ne!(a.id(), a_nested.id());
    }
}
