use crate::process::CommandExt;
use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};

/// Rewrites absolute paths into a form that is the same in every checkout
/// and on every machine, so that cache keys agree wherever a project is
/// built: a path inside the repository becomes relative to its root, and
/// any other path under the home directory becomes `~/`-relative. Other
/// paths stay absolute, which costs a cache miss elsewhere but never a
/// wrong hit. Outside Git the project root stands in for the repository.
///
/// All three roots are canonical because providers report canonical paths;
/// a symlinked home directory, for example, would otherwise never match.
#[derive(Debug, Clone)]
pub struct PortablePaths {
    /// Where `BUILD.bit` is, which relative paths are relative to.
    project: PathBuf,
    repo: PathBuf,
    home: Option<PathBuf>,
}

impl PortablePaths {
    pub fn detect(project_root: &Path) -> Self {
        let project = canonical(project_root);
        let repo = git_toplevel(&project).unwrap_or_else(|| project.clone());
        let home = dirs::home_dir().map(|home| canonical(&home));
        Self { project, repo, home }
    }

    pub fn repo(&self) -> &Path {
        &self.repo
    }

    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// The portable form of `s` when it is an absolute path. Anything else,
    /// including a relative path, is returned as it is, because a string
    /// that is not absolute cannot be told apart from one that is not a
    /// path at all.
    pub fn portable<'a>(&self, s: &'a str) -> Cow<'a, str> {
        if !s.starts_with('/') {
            return s.into();
        }
        let path = lexical(Path::new(s));
        if let Ok(rest) = path.strip_prefix(&self.repo) {
            return or_dot(rest, "").into();
        }
        if let Some(home) = &self.home
            && let Ok(rest) = path.strip_prefix(home)
        {
            return or_dot(rest, "~/").into();
        }
        s.into()
    }

    /// The portable form of a path a provider reports. A relative one is
    /// relative to the project root, where bit runs, so it is made absolute
    /// first; in a project below the repository root its portable form then
    /// carries the project's own location.
    pub fn portable_path<'a>(&self, s: &'a str) -> Cow<'a, str> {
        if s.starts_with('/') || self.project == self.repo && is_plain_relative(s) {
            return self.portable(s);
        }
        let absolute = self.project.join(s);
        match absolute.to_str() {
            Some(absolute) => self.portable(absolute).into_owned().into(),
            None => s.into(),
        }
    }

    /// The portable form of an environment variable's value or a line of a
    /// tool's version output: a single absolute path, or a `:`-separated
    /// list of them such as `PATH`. Any other value is returned as it is.
    pub fn portable_value<'a>(&self, value: &'a str) -> Cow<'a, str> {
        if !value.split(':').all(|part| part.starts_with('/')) {
            return value.into();
        }
        let parts: Vec<Cow<str>> = value.split(':').map(|part| self.portable(part)).collect();
        parts.join(":").into()
    }

    /// Make a path [`PortablePaths::portable_path`] produced absolute again.
    pub fn absolute(&self, s: &str) -> PathBuf {
        match (s.strip_prefix('~'), &self.home) {
            (Some(""), Some(home)) => home.clone(),
            (Some(rest), Some(home)) if rest.starts_with('/') => home.join(&rest[1..]),
            _ if s == "." => self.repo.clone(),
            _ => self.repo.join(s),
        }
    }
}

/// `rest` as a string, prefixed, or `.` (`~` for the home directory) when
/// it is empty.
fn or_dot(rest: &Path, prefix: &str) -> String {
    match (rest.as_os_str().is_empty(), prefix) {
        (true, "") => ".".to_owned(),
        (true, _) => prefix.trim_end_matches('/').to_owned(),
        (false, _) => format!("{prefix}{}", rest.display()),
    }
}

/// A relative path with no `.` or `..` component, which needs no rewriting
/// when the project is the repository root.
fn is_plain_relative(s: &str) -> bool {
    Path::new(s).components().all(|c| matches!(c, Component::Normal(_)))
}

/// `path` with `.` components removed and `..` applied, without touching
/// the filesystem.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The canonical root of the Git worktree containing `dir`.
fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    let output = crate::git::command()
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .logged_output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let toplevel = String::from_utf8_lossy(&output.stdout);
    let toplevel = toplevel.trim();
    (!toplevel.is_empty()).then(|| canonical(Path::new(toplevel)))
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

    /// A project in `sub/` of a repository at `/work/repo`, for a user whose
    /// home directory is `/home/me`.
    fn nested() -> PortablePaths {
        PortablePaths {
            project: "/work/repo/sub".into(),
            repo: "/work/repo".into(),
            home: Some("/home/me".into()),
        }
    }

    #[test]
    fn absolute_paths_become_repo_or_home_relative() {
        let paths = nested();
        assert_eq!(paths.portable("/work/repo/sub/src/main.rs"), "sub/src/main.rs");
        assert_eq!(paths.portable("/work/repo/lib/../go.work"), "go.work");
        assert_eq!(paths.portable("/work/repo"), ".");
        assert_eq!(paths.portable("/home/me/.cargo/config.toml"), "~/.cargo/config.toml");
        assert_eq!(paths.portable("/home/me"), "~");
        assert_eq!(paths.portable("/opt/tool"), "/opt/tool");
        assert_eq!(
            paths.portable("/work/repo2/x"),
            "/work/repo2/x",
            "a sibling is not inside"
        );
        assert_eq!(paths.portable("src/main.rs"), "src/main.rs");
        assert_eq!(paths.portable("go build"), "go build");
    }

    #[test]
    fn relative_paths_are_relative_to_the_project() {
        let paths = nested();
        assert_eq!(paths.portable_path("src/main.rs"), "sub/src/main.rs");
        assert_eq!(paths.portable_path("../shared/lib.rs"), "shared/lib.rs");
        assert_eq!(paths.portable_path("."), "sub");
        assert_eq!(paths.portable_path("/home/me/x"), "~/x");
    }

    #[test]
    fn absolute_reverses_portable_path() {
        let paths = nested();
        for path in [
            "/work/repo/sub/src/main.rs",
            "/work/repo",
            "/home/me/.cargo/config.toml",
            "/home/me",
            "/opt/tool",
        ] {
            assert_eq!(paths.absolute(&paths.portable_path(path)), Path::new(path), "{path}");
        }
    }

    #[test]
    fn path_values_and_lists_of_them_are_rewritten() {
        let paths = nested();
        assert_eq!(paths.portable_value("/home/me/.cargo"), "~/.cargo");
        assert_eq!(paths.portable_value("/work/repo/bin:/usr/bin"), "bin:/usr/bin");
        assert_eq!(paths.portable_value("-L /home/me/lib"), "-L /home/me/lib");
        assert_eq!(paths.portable_value("/home/me/x:relative"), "/home/me/x:relative");
        assert_eq!(paths.portable_value(""), "");
    }

    #[test]
    fn outside_git_the_project_is_the_repository() {
        let dir = tempfile::tempdir().unwrap();
        let paths = PortablePaths::detect(dir.path());
        assert_eq!(paths.repo(), dir.path().canonicalize().unwrap());
    }

    #[test]
    fn checkouts_of_one_repository_agree() {
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

        let a = PortablePaths::detect(&main.join("nested"));
        let b = PortablePaths::detect(&linked.join("nested"));
        assert_eq!(a.repo(), main.canonicalize().unwrap());
        assert_eq!(b.repo(), linked.canonicalize().unwrap());
        assert_eq!(a.portable_path("src/x"), "nested/src/x");
        assert_eq!(a.portable_path("src/x"), b.portable_path("src/x"));
    }
}
