//! Spawning `git` independently of any calling git hook.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Variables git exports to hooks that pin a repository regardless of the
/// working directory.
const HOOK_CONTEXT: [&str; 7] = [
    "GIT_DIR",
    "GIT_COMMON_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_PREFIX",
];

/// A `git` command whose repository is determined by its working directory.
///
/// When bit (or its test suite) runs from a git hook, git has exported
/// `GIT_DIR` and friends for the hook's repository. Left in place they would
/// redirect every git call, from project identity detection to import
/// fetches into bare clones, at that repository instead of the directory
/// the command is run in.
pub fn command() -> Command {
    let mut cmd = Command::new("git");
    for var in HOOK_CONTEXT {
        cmd.env_remove(var);
    }
    cmd
}

/// Paths changed between the merge base of a requested ref and the worktree.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// Every changed path. Renames include both the old and new paths.
    pub paths: HashSet<PathBuf>,
    /// Paths which no longer exist at their old location.
    pub removed: HashSet<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum ChangesError {
    #[error("failed to run git {operation}: {source}")]
    Spawn {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("git {operation} failed: {detail}")]
    Command { operation: &'static str, detail: String },
    #[error("git diff returned malformed --name-status output")]
    MalformedDiff,
}

/// Return paths changed since the merge base of `base` and `HEAD`, including
/// staged, unstaged, and untracked worktree changes.
///
/// Paths are absolute so callers rooted below the repository can still match
/// tracked inputs outside their project directory.
pub fn changes_since(project_root: &Path, base: &str) -> Result<Changes, ChangesError> {
    let root_output = command()
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(project_root)
        .output()
        .map_err(|source| ChangesError::Spawn {
            operation: "rev-parse",
            source,
        })?;
    if !root_output.status.success() {
        return Err(command_error("rev-parse", &root_output.stderr));
    }
    let repository_root = PathBuf::from(String::from_utf8_lossy(&root_output.stdout).trim());

    let merge_base_output = command()
        .args(["merge-base", "--", base, "HEAD"])
        .current_dir(&repository_root)
        .output()
        .map_err(|source| ChangesError::Spawn {
            operation: "merge-base",
            source,
        })?;
    if !merge_base_output.status.success() {
        return Err(command_error("merge-base", &merge_base_output.stderr));
    }
    let merge_base = String::from_utf8_lossy(&merge_base_output.stdout);
    let merge_base = merge_base.trim();

    let diff_output = command()
        .args(["diff", "--name-status", "-z", "--find-renames", merge_base, "--"])
        .current_dir(&repository_root)
        .output()
        .map_err(|source| ChangesError::Spawn {
            operation: "diff",
            source,
        })?;
    if !diff_output.status.success() {
        return Err(command_error("diff", &diff_output.stderr));
    }
    let mut changes = parse_name_status(&repository_root, &diff_output.stdout)?;

    let untracked_output = command()
        .args(["ls-files", "--others", "--exclude-standard", "-z", "--"])
        .current_dir(&repository_root)
        .output()
        .map_err(|source| ChangesError::Spawn {
            operation: "ls-files",
            source,
        })?;
    if !untracked_output.status.success() {
        return Err(command_error("ls-files", &untracked_output.stderr));
    }
    for path in untracked_output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        changes
            .paths
            .insert(repository_root.join(String::from_utf8_lossy(path).as_ref()));
    }
    Ok(changes)
}

fn command_error(operation: &'static str, stderr: &[u8]) -> ChangesError {
    let detail = String::from_utf8_lossy(stderr).trim().to_owned();
    ChangesError::Command {
        operation,
        detail: if detail.is_empty() {
            "unknown error".to_owned()
        } else {
            detail
        },
    }
}

fn parse_name_status(repository_root: &Path, bytes: &[u8]) -> Result<Changes, ChangesError> {
    let fields: Vec<&[u8]> = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect();
    let mut changes = Changes::default();
    let mut index = 0;
    while index < fields.len() {
        let status = fields[index];
        index += 1;
        let kind = status.first().copied().ok_or(ChangesError::MalformedDiff)?;
        match kind {
            b'R' | b'C' => {
                let old = fields.get(index).ok_or(ChangesError::MalformedDiff)?;
                let new = fields.get(index + 1).ok_or(ChangesError::MalformedDiff)?;
                index += 2;
                let old = repository_root.join(String::from_utf8_lossy(old).as_ref());
                let new = repository_root.join(String::from_utf8_lossy(new).as_ref());
                changes.paths.insert(old.clone());
                changes.paths.insert(new);
                if kind == b'R' {
                    changes.removed.insert(old);
                }
            }
            _ => {
                let path = fields.get(index).ok_or(ChangesError::MalformedDiff)?;
                index += 1;
                let path = repository_root.join(String::from_utf8_lossy(path).as_ref());
                changes.paths.insert(path.clone());
                if kind == b'D' {
                    changes.removed.insert(path);
                }
            }
        }
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_hook_repository_context() {
        let dir = tempfile::tempdir().unwrap();
        let out = command()
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(dir.path())
            .env("GIT_DIR", env!("CARGO_MANIFEST_DIR"))
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "a plain directory must not look like a repository"
        );
    }

    #[test]
    fn parses_modified_deleted_and_renamed_paths() {
        let root = Path::new("/repo");
        let changes = parse_name_status(root, b"M\0a.txt\0D\0old.txt\0R100\0before.txt\0after.txt\0").unwrap();

        assert_eq!(
            changes.paths,
            HashSet::from([
                root.join("a.txt"),
                root.join("old.txt"),
                root.join("before.txt"),
                root.join("after.txt"),
            ])
        );
        assert_eq!(
            changes.removed,
            HashSet::from([root.join("old.txt"), root.join("before.txt")])
        );
    }
}
