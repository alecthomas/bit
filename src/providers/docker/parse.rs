use std::collections::HashMap;
use std::path::{Path, PathBuf};

use parse_dockerfile::Instruction;

use crate::provider::BoxError;

/// Return Docker's built-in platform ARGs: `BUILD*` from the host, and
/// `TARGET*` from `target`, an `os/arch[/variant]` platform, or the host's
/// when the build names none.
///
/// See https://docs.docker.com/build/building/variables/#pre-defined-build-arguments
pub fn builtin_args(target: Option<&str>) -> HashMap<String, String> {
    let (os, arch, variant) = host_platform();
    let mut target_parts = target.unwrap_or_default().splitn(3, '/');
    let (target_os, target_arch, target_variant) = match (target_parts.next(), target_parts.next()) {
        (Some(target_os), Some(target_arch)) => (
            target_os.to_owned(),
            target_arch.to_owned(),
            target_parts.next().unwrap_or_default().to_owned(),
        ),
        _ => (os.clone(), arch.clone(), variant.clone()),
    };
    let platform = |os: &str, arch: &str, variant: &str| match variant {
        "" => format!("{os}/{arch}"),
        _ => format!("{os}/{arch}/{variant}"),
    };
    HashMap::from([
        ("BUILDPLATFORM".into(), platform(&os, &arch, &variant)),
        ("BUILDOS".into(), os),
        ("BUILDARCH".into(), arch),
        ("BUILDVARIANT".into(), variant),
        (
            "TARGETPLATFORM".into(),
            platform(&target_os, &target_arch, &target_variant),
        ),
        ("TARGETOS".into(), target_os),
        ("TARGETARCH".into(), target_arch),
        ("TARGETVARIANT".into(), target_variant),
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

type Vars = HashMap<String, String>;

/// Parse a Dockerfile and return local source paths from COPY/ADD instructions.
///
/// Variables follow Docker's scoping. ARGs before the first `FROM` are
/// global, and a stage sees one only when it declares it again. Each stage
/// has its own ARGs and ENVs, and inherits the ENVs of an earlier stage it
/// builds `FROM`. `build_args` override declared ARGs. ENVs override ARGs of
/// the same name, and an unset variable expands to nothing, as in Docker.
/// Skips multi-stage `COPY --from=...` and ADD with URLs.
///
/// A build for several `platforms` reads each platform's files, so the
/// sources of every one are returned. With none, Docker builds for the
/// host's platform.
pub fn dockerfile_sources(
    dockerfile: &Path,
    context: &Path,
    build_args: &HashMap<String, String>,
    platforms: &[String],
) -> Result<Vec<PathBuf>, BoxError> {
    let contents = std::fs::read_to_string(dockerfile).map_err(|e| format!("reading {}: {e}", dockerfile.display()))?;
    let parsed = parse_dockerfile::parse(&contents).map_err(|e| format!("parsing {}: {e}", dockerfile.display()))?;

    let targets: Vec<Option<&str>> = match platforms {
        [] => vec![None],
        platforms => platforms.iter().map(|platform| Some(platform.as_str())).collect(),
    };
    let mut paths = Vec::new();
    for target in targets {
        for path in platform_sources(&parsed, context, build_args, target)? {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    Ok(paths)
}

/// The sources [`dockerfile_sources`] returns for one `target` platform.
fn platform_sources(
    parsed: &parse_dockerfile::Dockerfile<'_>,
    context: &Path,
    build_args: &HashMap<String, String>,
    target: Option<&str>,
) -> Result<Vec<PathBuf>, BoxError> {
    let meta = builtin_args(target);
    let mut global = meta.clone();
    for arg in parsed.global_args() {
        for (name, default) in arg_words(&arg.arguments.value, &global)? {
            match build_args.get(&name).cloned().or(default) {
                Some(value) => global.insert(name, value),
                None => continue,
            };
        }
    }

    let mut stage_envs: HashMap<String, Vars> = HashMap::new();
    let mut paths = Vec::new();
    for stage in parsed.stages() {
        let base = expand(&stage.from.image.value, &global)?.to_ascii_lowercase();
        let mut env = stage_envs.get(&base).cloned().unwrap_or_default();
        let mut args = Vars::new();
        for instruction in stage.instructions {
            // ENV overrides an ARG of the same name.
            let mut vars = args.clone();
            vars.extend(env.iter().map(|(name, value)| (name.clone(), value.clone())));
            match instruction {
                Instruction::Arg(arg) => {
                    for (name, default) in arg_words(&arg.arguments.value, &vars)? {
                        let value = build_args
                            .get(&name)
                            .cloned()
                            .or(default)
                            .or_else(|| global.get(&name).cloned());
                        if let Some(value) = value {
                            args.insert(name, value);
                        }
                    }
                }
                Instruction::Env(instruction) => env.extend(env_pairs(&instruction.arguments.value, &vars)?),
                Instruction::Copy(copy) => {
                    let has_from = copy.options.iter().any(|f| &*f.name.value == "from");
                    if has_from {
                        continue;
                    }
                    for src in &copy.src {
                        if let parse_dockerfile::Source::Path(p) = src {
                            paths.push(context.join(expand(&p.value, &vars)?));
                        }
                    }
                }
                Instruction::Add(add) => {
                    for src in &add.src {
                        if let parse_dockerfile::Source::Path(p) = src {
                            let src_path = expand(&p.value, &vars)?;
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
        if let Some((_, name)) = &stage.from.as_ {
            stage_envs.insert(name.value.to_ascii_lowercase(), env);
        }
    }
    Ok(paths)
}

/// The names an `ARG` instruction declares, each with its default, if it
/// gives one, expanded with `vars`. One instruction may declare several.
fn arg_words(raw: &str, vars: &Vars) -> Result<Vec<(String, Option<String>)>, BoxError> {
    Ok(words(raw, vars)?
        .into_iter()
        .map(|word| match word.split_once('=') {
            Some((name, default)) => (name.to_owned(), Some(default.to_owned())),
            None => (word, None),
        })
        .collect())
}

/// The variables an `ENV` instruction sets, expanded with `vars`: any
/// number of `name=value` words, or the legacy `ENV name value` form,
/// whose value is the rest of the line.
fn env_pairs(raw: &str, vars: &Vars) -> Result<Vec<(String, String)>, BoxError> {
    let raw = raw.trim();
    let legacy_name = raw.split_whitespace().next().filter(|name| !name.contains('='));
    if let Some(name) = legacy_name {
        let value = words(raw[name.len()..].trim(), vars)?.join(" ");
        return Ok(vec![(name.to_owned(), value)]);
    }
    Ok(words(raw, vars)?
        .into_iter()
        .filter_map(|word| {
            word.split_once('=')
                .map(|(name, value)| (name.to_owned(), value.to_owned()))
        })
        .collect())
}

/// Split `raw` into words as Docker's shell lexer does, expanding variables
/// with `vars`: single quotes keep their contents literally, double quotes
/// keep spaces and still expand, and a backslash escapes the next character.
fn words(raw: &str, vars: &Vars) -> Result<Vec<String>, BoxError> {
    let mut words = Vec::new();
    let mut chars = raw.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.peek().is_none() {
            return Ok(words);
        }
        let mut word = String::new();
        // Text not yet expanded, which is expanded whole so `${…}` spans it.
        let mut pending = String::new();
        while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
            match c {
                '\'' => {
                    word.push_str(&expand(&std::mem::take(&mut pending), vars)?);
                    word.extend(chars.by_ref().take_while(|&c| c != '\''));
                }
                '"' => {
                    word.push_str(&expand(&std::mem::take(&mut pending), vars)?);
                    while let Some(c) = chars.next() {
                        match c {
                            '"' => break,
                            '\\' if chars.peek().is_some_and(|next| matches!(next, '"' | '\\' | '$')) => {
                                word.push_str(&expand(&std::mem::take(&mut pending), vars)?);
                                word.extend(chars.next());
                            }
                            _ => pending.push(c),
                        }
                    }
                    word.push_str(&expand(&std::mem::take(&mut pending), vars)?);
                }
                '\\' => {
                    word.push_str(&expand(&std::mem::take(&mut pending), vars)?);
                    word.extend(chars.next());
                }
                _ => pending.push(c),
            }
        }
        word.push_str(&expand(&pending, vars)?);
        words.push(word);
    }
}

/// Expand `$VAR` and `${VAR}` references using `vars`, with BuildKit's
/// `${VAR:-word}` and `${VAR-word}` defaults and `${VAR:+word}` and
/// `${VAR+word}` alternatives; the forms with `:` treat an empty variable
/// as unset. An unset variable expands to nothing, as in Docker.
fn expand(text: &str, vars: &Vars) -> Result<String, BoxError> {
    let is_name = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = String::new();
    let mut rest = text;
    while let Some(dollar) = rest.find('$') {
        out.push_str(&rest[..dollar]);
        rest = &rest[dollar + 1..];
        if let Some(braced) = rest.strip_prefix('{') {
            let end = braced
                .find('}')
                .ok_or_else(|| format!("unterminated variable in {text:?}"))?;
            let expr = &braced[..end];
            rest = &braced[end + 1..];
            let name_end = expr.find(|c| !is_name(c)).unwrap_or(expr.len());
            let (name, op) = expr.split_at(name_end);
            let value = vars.get(name);
            let set = |empty_is_unset: bool| value.is_some_and(|v| !(empty_is_unset && v.is_empty()));
            let expanded = if op.is_empty() {
                value.cloned().unwrap_or_default()
            } else if let Some(word) = op.strip_prefix(":-") {
                if set(true) {
                    value.cloned().unwrap_or_default()
                } else {
                    expand(word, vars)?
                }
            } else if let Some(word) = op.strip_prefix(":+") {
                if set(true) { expand(word, vars)? } else { String::new() }
            } else if let Some(word) = op.strip_prefix('-') {
                if set(false) {
                    value.cloned().unwrap_or_default()
                } else {
                    expand(word, vars)?
                }
            } else if let Some(word) = op.strip_prefix('+') {
                if set(false) { expand(word, vars)? } else { String::new() }
            } else {
                return Err(format!("unsupported variable expression ${{{expr}}} in {text:?}").into());
            };
            out.push_str(&expanded);
        } else {
            let name_end = rest.find(|c| !is_name(c)).unwrap_or(rest.len());
            if name_end == 0 {
                out.push('$');
                continue;
            }
            out.push_str(vars.get(&rest[..name_end]).map_or("", String::as_str));
            rest = &rest[name_end..];
        }
    }
    out.push_str(rest);
    Ok(out)
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

        Some(Self {
            context: context.to_path_buf(),
            ignore,
            negate,
        })
    }

    /// Returns true if a path should be excluded by .dockerignore rules.
    pub fn is_ignored(&self, path: &Path) -> bool {
        let rel = path.strip_prefix(&self.context).unwrap_or(path);
        self.ignore.iter().any(|p| p.matches_path(rel)) && !self.negate.iter().any(|p| p.matches_path(rel))
    }
}

/// Expand a path (file, directory, or glob) into individual files,
/// filtering out paths matched by .dockerignore.
pub fn expand_path(path: &Path, dockerignore: &Option<DockerIgnore>) -> Vec<PathBuf> {
    let pattern = path.to_string_lossy();
    let filter = |e: &PathBuf| -> bool { e.is_file() && !dockerignore.as_ref().is_some_and(|di| di.is_ignored(e)) };

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
    fn expand_with_known_var() {
        let vars = HashMap::from([("ARCH".into(), "arm64".into())]);
        assert_eq!(expand("dist/app-${ARCH}", &vars).unwrap(), "dist/app-arm64");
    }

    #[test]
    fn expand_unbraced() {
        let vars = HashMap::from([("ARCH".into(), "amd64".into())]);
        assert_eq!(expand("dist/app-$ARCH", &vars).unwrap(), "dist/app-amd64");
    }

    #[test]
    fn expand_unknown_is_empty() {
        let vars = HashMap::new();
        assert_eq!(expand("dist/app-${TARGETARCH}", &vars).unwrap(), "dist/app-");
        assert_eq!(expand("dist/${MODE:-release}", &vars).unwrap(), "dist/release");
        let vars = HashMap::from([("EMPTY".into(), String::new()), ("SET".into(), "s".into())]);
        assert_eq!(
            expand("${EMPTY:-d}|${EMPTY-d}|${SET:+alt}|${EMPTY+alt}|$", &vars).unwrap(),
            "d||alt|alt|$"
        );
    }

    #[test]
    fn expand_no_vars() {
        assert_eq!(expand("dist/app", &HashMap::new()).unwrap(), "dist/app");
    }

    #[test]
    fn expand_multiple() {
        let vars = HashMap::from([("OS".into(), "linux".into()), ("ARCH".into(), "arm64".into())]);
        assert_eq!(expand("${OS}/${ARCH}/bin", &vars).unwrap(), "linux/arm64/bin");
    }

    #[test]
    fn words_follow_shell_quoting() {
        let vars = HashMap::from([("V".into(), "x".into())]);
        assert_eq!(
            words(r#"A="a $V b" B='$V c' C=d\ e D=$V"#, &vars).unwrap(),
            ["A=a x b", "B=$V c", "C=d e", "D=x"]
        );
    }

    fn sources(dockerfile: &str, build_args: &[(&str, &str)]) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), dockerfile).unwrap();
        let build_args = build_args.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect();
        dockerfile_sources(&dir.path().join("Dockerfile"), Path::new(""), &build_args, &[])
            .unwrap()
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn arg_and_env_take_several_quoted_pairs() {
        let dockerfile = "FROM alpine\nARG A=\"one\" B=two\nENV C=\"$A dir\" D=${B}\nCOPY $C/x $D/y /app/\n";
        assert_eq!(sources(dockerfile, &[]), ["one dir/x", "two/y"]);
        assert_eq!(sources(dockerfile, &[("B", "three")]), ["one dir/x", "three/y"]);
        assert_eq!(
            sources("FROM alpine\nENV DIR some dir\nCOPY $DIR /app/\n", &[]),
            ["some dir"]
        );
    }

    #[test]
    fn variables_are_scoped_to_their_stage() {
        let dockerfile = concat!(
            "ARG VERSION=1\n",
            "FROM alpine AS base\n",
            "ENV ROOT=base\n",
            "ARG LOCAL=local\n",
            "COPY v$VERSION /\n",
            "FROM base\n",
            "ARG VERSION\n",
            "COPY $ROOT/$LOCAL/v$VERSION /\n",
            "FROM alpine\n",
            "COPY $ROOT/x /\n",
        );
        // A global ARG is unset in a stage until redeclared, ENV is inherited
        // from the stage built on, and ARG is not.
        assert_eq!(sources(dockerfile, &[]), ["v", "base//v1", "/x"]);
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

        let sources = dockerfile_sources(&dockerfile, dir.path(), &HashMap::new(), &[]).unwrap();
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
        let sources = dockerfile_sources(&dockerfile, dir.path(), &args, &[]).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0], dir.path().join("dist/app-arm64"));
    }

    #[test]
    fn dockerfile_unset_var_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        std::fs::write(
            &dockerfile,
            "FROM alpine\nARG CUSTOM_VAR\nCOPY dist/app-${CUSTOM_VAR} /usr/bin/app\n",
        )
        .unwrap();

        let sources = dockerfile_sources(&dockerfile, dir.path(), &HashMap::new(), &[]).unwrap();
        assert_eq!(sources, [dir.path().join("dist/app-")]);
    }

    #[test]
    fn target_args_come_from_the_platforms_built() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        std::fs::write(
            &dockerfile,
            "FROM alpine\nARG TARGETOS TARGETARCH TARGETVARIANT\nCOPY dist/$TARGETOS-$TARGETARCH$TARGETVARIANT /app\n",
        )
        .unwrap();

        let platforms = ["linux/amd64".to_owned(), "linux/arm/v7".to_owned()];
        let sources = dockerfile_sources(&dockerfile, dir.path(), &HashMap::new(), &platforms).unwrap();
        assert_eq!(
            sources,
            [dir.path().join("dist/linux-amd64"), dir.path().join("dist/linux-armv7")]
        );
        let args = builtin_args(Some("windows/arm64"));
        assert_eq!(args["TARGETPLATFORM"], "windows/arm64");
        assert_eq!(args["BUILDPLATFORM"], builtin_args(None)["TARGETPLATFORM"]);
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

        let sources = dockerfile_sources(&dockerfile, dir.path(), &HashMap::new(), &[]).unwrap();
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

        let sources = dockerfile_sources(&dockerfile, dir.path(), &HashMap::new(), &[]).unwrap();
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

        let sources = dockerfile_sources(&dockerfile, dir.path(), &HashMap::new(), &[]).unwrap();
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

        let sources = dockerfile_sources(&dockerfile, dir.path(), &HashMap::new(), &[]).unwrap();
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
