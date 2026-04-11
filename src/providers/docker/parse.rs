use std::collections::HashMap;
use std::path::{Path, PathBuf};

use parse_dockerfile::Instruction;

use crate::provider::BoxError;

/// Return Docker's built-in platform ARGs based on the host architecture.
///
/// See https://docs.docker.com/build/building/variables/#pre-defined-build-arguments
pub fn builtin_args() -> HashMap<String, String> {
    let (os, arch, variant) = host_platform();
    let platform = if variant.is_empty() {
        format!("{os}/{arch}")
    } else {
        format!("{os}/{arch}/{variant}")
    };
    HashMap::from([
        ("BUILDPLATFORM".into(), platform.clone()),
        ("BUILDOS".into(), os.clone()),
        ("BUILDARCH".into(), arch.clone()),
        ("BUILDVARIANT".into(), variant.clone()),
        ("TARGETPLATFORM".into(), platform),
        ("TARGETOS".into(), os),
        ("TARGETARCH".into(), arch),
        ("TARGETVARIANT".into(), variant),
    ])
}

/// Map the host OS and architecture to Docker platform values.
fn host_platform() -> (String, String, String) {
    let os = match std::env::consts::OS {
        "macos" => "linux", // Docker on macOS builds Linux images
        other => other,
    };
    let (arch, variant) = match std::env::consts::ARCH {
        "x86_64" => ("amd64", ""),
        "aarch64" => ("arm64", ""),
        "arm" => ("arm", "v7"),
        "s390x" => ("s390x", ""),
        "powerpc64" => ("ppc64le", ""),
        "riscv64" => ("riscv64", ""),
        other => (other, ""),
    };
    (os.into(), arch.into(), variant.into())
}

/// Parse a Dockerfile and return local source paths from COPY/ADD instructions.
///
/// Collects ARG/ENV defaults from the Dockerfile, merges with `build_args`,
/// and expands `$VAR`/`${VAR}` references in COPY/ADD source paths.
/// Skips multi-stage `COPY --from=...` and ADD with URLs.
pub fn dockerfile_sources(
    dockerfile: &Path,
    context: &Path,
    build_args: &HashMap<String, String>,
) -> Result<Vec<PathBuf>, BoxError> {
    let contents = std::fs::read_to_string(dockerfile)
        .map_err(|e| format!("reading {}: {e}", dockerfile.display()))?;
    let parsed = parse_dockerfile::parse(&contents)
        .map_err(|e| format!("parsing {}: {e}", dockerfile.display()))?;

    let mut vars = builtin_args();
    for instruction in &parsed.instructions {
        match instruction {
            Instruction::Arg(arg) => {
                let raw = &*arg.arguments.value;
                if let Some((name, default)) = raw.split_once('=') {
                    vars.insert(name.trim().to_owned(), default.trim().to_owned());
                }
            }
            Instruction::Env(env) => {
                for pair in env.arguments.value.split_whitespace() {
                    if let Some((name, val)) = pair.split_once('=') {
                        vars.insert(name.to_owned(), val.to_owned());
                    }
                }
            }
            _ => {}
        }
    }
    for (k, v) in build_args {
        vars.insert(k.clone(), v.clone());
    }

    let mut paths = Vec::new();
    for instruction in &parsed.instructions {
        match instruction {
            Instruction::Copy(copy) => {
                let has_from = copy.options.iter().any(|f| &*f.name.value == "from");
                if has_from {
                    continue;
                }
                for src in &copy.src {
                    if let parse_dockerfile::Source::Path(p) = src {
                        let src_path = expand_vars(&p.value, &vars)?;
                        paths.push(context.join(src_path));
                    }
                }
            }
            Instruction::Add(add) => {
                for src in &add.src {
                    if let parse_dockerfile::Source::Path(p) = src {
                        let src_path = expand_vars(&p.value, &vars)?;
                        if src_path.starts_with("http://") || src_path.starts_with("https://") {
                            continue;
                        }
                        paths.push(context.join(src_path));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(paths)
}

/// Expand `$VAR`, `${VAR}`, `${VAR:-default}`, and `${VAR:+replacement}`
/// references using the provided variable map.
/// Returns an error if a variable is unresolved.
pub fn expand_vars(path: &str, vars: &HashMap<String, String>) -> Result<String, BoxError> {
    shellexpand::env_with_context(path, |name| -> Result<Option<std::borrow::Cow<'_, str>>, BoxError> {
        match vars.get(name) {
            Some(v) => Ok(Some(v.as_str().into())),
            None => Err(format!("unresolved Dockerfile variable: ${name}").into()),
        }
    })
    .map(|s| s.into_owned())
    .map_err(|e| e.cause)
}

/// .dockerignore pattern matcher.
pub struct DockerIgnore {
    context: PathBuf,
    ignore: Vec<glob::Pattern>,
    negate: Vec<glob::Pattern>,
}

impl DockerIgnore {
    /// Load a .dockerignore from the build context directory.
    /// Returns `None` if no .dockerignore exists.
    pub fn load(context: &Path) -> Option<Self> {
        let contents = std::fs::read_to_string(context.join(".dockerignore")).ok()?;
        let mut ignore = Vec::new();
        let mut negate = Vec::new();

        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (pattern, is_negation) = if let Some(rest) = line.strip_prefix('!') {
                (rest.trim(), true)
            } else {
                (line, false)
            };

            let glob_pattern = if pattern.contains('/') {
                pattern.to_owned()
            } else {
                format!("**/{pattern}")
            };

            let Ok(pat) = glob::Pattern::new(&glob_pattern) else {
                continue;
            };
            let dir_pat = glob::Pattern::new(&format!("{glob_pattern}/**")).ok();
            if is_negation {
                negate.push(pat);
                if let Some(dp) = dir_pat {
                    negate.push(dp);
                }
            } else {
                ignore.push(pat);
                if let Some(dp) = dir_pat {
                    ignore.push(dp);
                }
            }
        }

        Some(Self { context: context.to_path_buf(), ignore, negate })
    }

    /// Returns true if a path should be excluded by .dockerignore rules.
    pub fn is_ignored(&self, path: &Path) -> bool {
        let rel = path.strip_prefix(&self.context).unwrap_or(path);
        self.ignore.iter().any(|p| p.matches_path(rel))
            && !self.negate.iter().any(|p| p.matches_path(rel))
    }
}

/// Expand a path (file, directory, or glob) into individual files,
/// filtering out paths matched by .dockerignore.
pub fn expand_path(path: &Path, dockerignore: &Option<DockerIgnore>) -> Vec<PathBuf> {
    let pattern = path.to_string_lossy();
    let filter = |e: &PathBuf| -> bool {
        e.is_file() && !dockerignore.as_ref().is_some_and(|di| di.is_ignored(e))
    };

    if path.is_dir() {
        let glob_pattern = format!("{}/**/*", pattern);
        if let Ok(entries) = glob::glob(&glob_pattern) {
            return entries.flatten().filter(|e| filter(e)).collect();
        }
        return vec![];
    }

    if let Ok(entries) = glob::glob(&pattern) {
        return entries.flatten().filter(|e| filter(e)).collect();
    }
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_vars_with_known_var() {
        let vars = HashMap::from([("ARCH".into(), "arm64".into())]);
        assert_eq!(expand_vars("dist/app-${ARCH}", &vars).unwrap(), "dist/app-arm64");
    }

    #[test]
    fn expand_vars_unbraced() {
        let vars = HashMap::from([("ARCH".into(), "amd64".into())]);
        assert_eq!(expand_vars("dist/app-$ARCH", &vars).unwrap(), "dist/app-amd64");
    }

    #[test]
    fn expand_vars_unknown_is_error() {
        let vars = HashMap::new();
        assert!(expand_vars("dist/app-${TARGETARCH}", &vars).is_err());
    }

    #[test]
    fn expand_vars_no_vars() {
        assert_eq!(expand_vars("dist/app", &HashMap::new()).unwrap(), "dist/app");
    }

    #[test]
    fn expand_vars_multiple() {
        let vars = HashMap::from([("OS".into(), "linux".into()), ("ARCH".into(), "arm64".into())]);
        assert_eq!(expand_vars("${OS}/${ARCH}/bin", &vars).unwrap(), "linux/arm64/bin");
    }

    #[test]
    fn dockerfile_expands_arg_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        std::fs::write(
            &dockerfile,
            "FROM alpine\nARG TARGETARCH=amd64\nCOPY dist/app-${TARGETARCH} /usr/bin/app\n",
        )
        .unwrap();

        let sources = dockerfile_sources(&dockerfile, dir.path(), &HashMap::new()).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0], dir.path().join("dist/app-amd64"));
    }

    #[test]
    fn dockerfile_build_args_override_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        std::fs::write(
            &dockerfile,
            "FROM alpine\nARG TARGETARCH=amd64\nCOPY dist/app-${TARGETARCH} /usr/bin/app\n",
        )
        .unwrap();

        let args = HashMap::from([("TARGETARCH".into(), "arm64".into())]);
        let sources = dockerfile_sources(&dockerfile, dir.path(), &args).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0], dir.path().join("dist/app-arm64"));
    }

    #[test]
    fn dockerfile_unknown_var_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        std::fs::write(
            &dockerfile,
            "FROM alpine\nARG CUSTOM_VAR\nCOPY dist/app-${CUSTOM_VAR} /usr/bin/app\n",
        )
        .unwrap();

        assert!(dockerfile_sources(&dockerfile, dir.path(), &HashMap::new()).is_err());
    }

    #[test]
    fn dockerfile_resolves_builtin_targetarch() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        std::fs::write(
            &dockerfile,
            "FROM alpine\nARG TARGETARCH\nCOPY dist/app-${TARGETARCH} /usr/bin/app\n",
        )
        .unwrap();

        let sources = dockerfile_sources(&dockerfile, dir.path(), &HashMap::new()).unwrap();
        assert_eq!(sources.len(), 1);
        let path_str = sources[0].to_string_lossy();
        assert!(!path_str.contains('$'));
        assert!(!path_str.contains('*'));
    }

    #[test]
    fn dockerfile_extracts_copy_sources() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        std::fs::write(
            &dockerfile,
            "FROM alpine\nCOPY src/ /app/src/\nCOPY config.toml /app/\n",
        )
        .unwrap();

        let sources = dockerfile_sources(&dockerfile, dir.path(), &HashMap::new()).unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0], dir.path().join("src/"));
        assert_eq!(sources[1], dir.path().join("config.toml"));
    }

    #[test]
    fn dockerfile_skips_multistage_copy() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        std::fs::write(
            &dockerfile,
            concat!(
                "FROM rust AS builder\n",
                "COPY src/ /src/\n",
                "FROM debian\n",
                "COPY --from=builder /src/target/app /usr/bin/app\n",
            ),
        )
        .unwrap();

        let sources = dockerfile_sources(&dockerfile, dir.path(), &HashMap::new()).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0], dir.path().join("src/"));
    }

    #[test]
    fn dockerfile_skips_add_urls() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        std::fs::write(
            &dockerfile,
            "FROM alpine\nADD https://example.com/file.tar.gz /app/\nADD local.txt /app/\n",
        )
        .unwrap();

        let sources = dockerfile_sources(&dockerfile, dir.path(), &HashMap::new()).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0], dir.path().join("local.txt"));
    }

    #[test]
    fn dockerignore_negation() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.log"), "log").unwrap();
        std::fs::write(src.join("important.log"), "keep").unwrap();
        std::fs::write(dir.path().join(".dockerignore"), "*.log\n!important.log\n").unwrap();

        let di = DockerIgnore::load(dir.path()).unwrap();
        assert!(di.is_ignored(&src.join("a.log")));
        assert!(!di.is_ignored(&src.join("important.log")));
    }

    #[test]
    fn dockerignore_subdirectory_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let build_dir = dir.path().join("src/build");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::write(build_dir.join("output.o"), "binary").unwrap();
        std::fs::write(dir.path().join(".dockerignore"), "src/build\n").unwrap();

        let di = DockerIgnore::load(dir.path()).unwrap();
        assert!(di.is_ignored(&build_dir.join("output.o")));
        assert!(!di.is_ignored(&dir.path().join("src/main.rs")));
    }
}
