//! Spawning `git` independently of any calling git hook.

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
}
